//! Regression test: SQLite reuses the highest row id after a delete. A new picture with a reused id must never be
//! served (or cached) under an old picture's URL.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chuck::{api, auth::Authenticator, blob::FsBlobStore, sqlite::SqliteStore};
use http_body_util::BodyExt;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

struct HeaderUser;
impl Authenticator for HeaderUser {
    fn authenticate(&self, h: &HeaderMap) -> Option<String> {
        h.get("x-user")?.to_str().ok().map(str::to_string)
    }
}

struct App {
    router: axum::Router,
    media: PathBuf,
}

fn app(name: &str) -> App {
    let media = std::env::temp_dir().join(format!("chuck-media-cache-test-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&media).ok();
    let state = api::AppState {
        store: Arc::new(SqliteStore::open(":memory:").unwrap()),
        auth: Arc::new(HeaderUser),
        blobs: Arc::new(FsBlobStore::open(&media).unwrap()),
    };
    App { router: api::router(state), media }
}

impl App {
    async fn call(&self, method: &str, uri: &str, user: Option<&str>, body: Vec<u8>, json: bool) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(u) = user {
            req = req.header("x-user", u);
        }
        if json {
            req = req.header("content-type", "application/json");
        }
        let resp = self.router.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
        let (parts, b) = resp.into_parts();
        (parts.status, parts.headers, b.collect().await.unwrap().to_bytes().to_vec())
    }

    async fn new_card(&self, user: &str, text: &str, media: bool) -> (StatusCode, serde_json::Value) {
        let uri = if media { "/api/notes?media=true" } else { "/api/notes" };
        let (s, _, b) = self.call("POST", uri, Some(user), serde_json::json!({ "body": text }).to_string().into_bytes(), true).await;
        (s, serde_json::from_slice(&b).unwrap_or_default())
    }

    async fn attach(&self, user: &str, note: i64, bytes: Vec<u8>, name: &str) -> (StatusCode, serde_json::Value) {
        let (s, _, b) = self.call("POST", &format!("/api/notes/{note}/attachments?name={name}"), Some(user), bytes, false).await;
        (s, serde_json::from_slice(&b).unwrap_or_default())
    }
}

fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, marker];
    v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// A structurally valid 800x300 JPEG. `salt` makes distinct files.
fn jpeg(salt: u8) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    j.extend(seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
    j.extend(seg(0xDB, &[salt; 65]));
    j.extend(seg(0xC0, &[8, 0x01, 0x2C, 0x03, 0x20, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]));
    j.extend(seg(0xDA, &[3, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]));
    j.extend_from_slice(&[0x12, salt, 0xFF, 0xD9]);
    j
}

#[tokio::test]
async fn reused_ids_never_serve_a_stale_picture() {
    let a = app("reuse");
    let card = a.new_card("alice", "pics", false).await.1["id"].as_i64().unwrap();
    let first = a.attach("alice", card, jpeg(1), "first.jpg").await.1;
    let (id1, v1) = (first["id"].as_i64().unwrap(), first["v"].as_str().unwrap().to_string());
    assert_eq!(v1.len(), 16);

    // the current version may be cached for long, anything else must be revalidated
    let (s, h, _) = a.call("GET", &format!("/api/media/{id1}?v={v1}"), Some("alice"), vec![], false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h["cache-control"].to_str().unwrap().contains("immutable"));
    let etag = h["etag"].to_str().unwrap().to_string();
    let (_, h, _) = a.call("GET", &format!("/api/media/{id1}"), Some("alice"), vec![], false).await;
    assert_eq!(h["cache-control"], "private, no-cache");

    // delete it and upload a different picture: SQLite hands out the same id again
    let (s, _, _) = a.call("DELETE", &format!("/api/media/{id1}"), Some("alice"), vec![], false).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let second = a.attach("alice", card, jpeg(2), "second.jpg").await.1;
    let (id2, v2) = (second["id"].as_i64().unwrap(), second["v"].as_str().unwrap().to_string());
    assert_eq!(id2, id1, "this is the reuse that caused the bug");
    assert_ne!(v2, v1, "but the version, and so the URL, is new");

    // a browser revalidating the old URL with the old ETag gets the new file, not 304
    let mut req = axum::http::Request::builder().method("GET").uri(format!("/api/media/{id2}?v={v1}")).header("x-user", "alice").header("if-none-match", &etag);
    req = req.header("accept", "*/*");
    let resp = a.router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["cache-control"], "private, no-cache", "an old version in the URL is never cached for long");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(body.ends_with(&[0x12, 2, 0xFF, 0xD9]), "the second picture is served");

    // the current ETag gives 304
    let (_, h, _) = a.call("GET", &format!("/api/media/{id2}?v={v2}"), Some("alice"), vec![], false).await;
    let req = axum::http::Request::builder().method("GET").uri(format!("/api/media/{id2}?v={v2}")).header("x-user", "alice").header("if-none-match", h["etag"].to_str().unwrap());
    let resp = a.router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

    // another user with the same id gets 404 even with a valid-looking version
    let (s, _, _) = a.call("GET", &format!("/api/media/{id2}?v={v2}"), Some("bob"), vec![], false).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(&a.media).ok();
}
