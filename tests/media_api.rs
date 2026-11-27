//! HTTP-level tests for media cards, run in-process against the real router: upload, metadata stripping, serving
//! headers, per-user isolation, deduplication and cleanup. Image bytes are built in memory.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chuck::{api, auth::Authenticator, blob::FsBlobStore, sqlite::SqliteStore};
use http_body_util::BodyExt;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

/// Test identity: the `x-user` header. Real deployments use one of the auth providers.
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
    let media = std::env::temp_dir().join(format!("chuck-media-test-{name}-{}", std::process::id()));
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
        let uri = format!("/api/notes/{note}/attachments?name={name}");
        let (s, _, b) = self.call("POST", &uri, Some(user), bytes, false).await;
        (s, serde_json::from_slice(&b).unwrap_or_default())
    }

    fn files(&self) -> usize {
        fn walk(p: &std::path::Path) -> usize {
            std::fs::read_dir(p).map_or(0, |d| d.flatten().map(|e| if e.path().is_dir() { walk(&e.path()) } else { 1 }).sum())
        }
        walk(&self.media)
    }
}

fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, marker];
    v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// A structurally valid 800x300 JPEG carrying EXIF GPS. `salt` makes distinct files.
fn jpeg(salt: u8) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    j.extend(seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
    j.extend(seg(0xE1, b"Exif\0\0GPSLatitude=40.7128N;GPSLongitude=74.0060W"));
    j.extend(seg(0xDB, &[salt; 65]));
    j.extend(seg(0xC0, &[8, 0x01, 0x2C, 0x03, 0x20, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]));
    j.extend(seg(0xDA, &[3, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]));
    j.extend_from_slice(&[0x12, salt, 0xFF, 0xD9]);
    j
}

fn png_thumb() -> Vec<u8> {
    let chunk = |ty: &[u8], d: &[u8]| {
        let mut v = (d.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(d);
        v.extend_from_slice(&[0; 4]);
        v
    };
    let mut ihdr = 320u32.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&120u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
    p.extend(chunk(b"IHDR", &ihdr));
    p.extend(chunk(b"IDAT", b"x"));
    p.extend(chunk(b"IEND", b""));
    p
}

#[tokio::test]
async fn upload_strips_metadata_and_serves_safely() {
    let a = app("serve");
    let (s, card) = a.new_card("alice", "", true).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = card["id"].as_i64().unwrap();
    assert_eq!(a.new_card("alice", "", false).await.0, StatusCode::BAD_REQUEST, "empty card needs the media flag");

    let (s, att) = a.attach("alice", id, jpeg(1), "..%2F..%2Fholiday.jpg").await;
    assert_eq!(s, StatusCode::CREATED, "{att}");
    assert_eq!((att["mime"].as_str(), att["width"].as_i64(), att["height"].as_i64()), (Some("image/jpeg"), Some(800), Some(300)));
    assert_eq!(att["name"], "holiday.jpg", "path components are dropped from names");
    assert!(att.get("blob_key").is_none(), "storage keys are never sent to clients");

    let aid = att["id"].as_i64().unwrap();
    let (s, h, body) = a.call("GET", &format!("/api/media/{aid}"), Some("alice"), vec![], false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!body.windows(11).any(|w| w == b"GPSLatitude"), "GPS must be stripped before storage");
    assert!(body.starts_with(&[0xFF, 0xD8, 0xFF]) && body.ends_with(&[0x12, 1, 0xFF, 0xD9]));
    assert_eq!(h["content-type"], "image/jpeg");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(h["content-security-policy"].to_str().unwrap().contains("sandbox"));
    assert!(h["cache-control"].to_str().unwrap().starts_with("private"));

    // the card now reports its attachment and kind
    let (_, _, list) = a.call("GET", "/api/notes", Some("alice"), vec![], false).await;
    let list: serde_json::Value = serde_json::from_slice(&list).unwrap();
    assert_eq!(list[0]["kind"], "media");
    assert_eq!(list[0]["attachments"].as_array().unwrap().len(), 1);

    // a preview is served when asked for, the original otherwise
    let (s, _, _) = a.call("PUT", &format!("/api/media/{aid}/thumb"), Some("alice"), png_thumb(), false).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, h, _) = a.call("GET", &format!("/api/media/{aid}?thumb=true"), Some("alice"), vec![], false).await;
    assert_eq!(h["content-type"], "image/png");
    let (_, h, _) = a.call("GET", &format!("/api/media/{aid}"), Some("alice"), vec![], false).await;
    assert_eq!(h["content-type"], "image/jpeg");
}

#[tokio::test]
async fn rejects_what_is_not_an_allowed_image() {
    let a = app("reject");
    let id = a.new_card("alice", "x", false).await.1["id"].as_i64().unwrap();
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg' onload='alert(1)'/>".to_vec();
    assert_eq!(a.attach("alice", id, svg, "x.jpg").await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE, "type comes from bytes, not the name");
    assert_eq!(a.attach("alice", id, b"%PDF-1.7".to_vec(), "a.pdf").await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(a.attach("alice", id, jpeg(1)[..40].to_vec(), "cut.jpg").await.0, StatusCode::BAD_REQUEST);
    let big = vec![0u8; chuck::media::MAX_IMAGE_BYTES + 4096];
    assert_eq!(a.attach("alice", id, big, "big.jpg").await.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(a.attach("alice", 999_999, jpeg(1), "x.jpg").await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.files(), 0, "nothing rejected is stored");
}

#[tokio::test]
async fn users_cannot_touch_each_others_media() {
    let a = app("isolation");
    let id = a.new_card("alice", "alice's", false).await.1["id"].as_i64().unwrap();
    let aid = a.attach("alice", id, jpeg(1), "a.jpg").await.1["id"].as_i64().unwrap();
    let m = format!("/api/media/{aid}");
    for (method, uri, body) in [
        ("GET", m.clone(), vec![]),
        ("GET", format!("{m}?thumb=true"), vec![]),
        ("DELETE", m.clone(), vec![]),
        ("PUT", format!("{m}/thumb"), png_thumb()),
        ("POST", format!("/api/notes/{id}/attachments"), jpeg(2)),
    ] {
        let (s, _, _) = a.call(method, &uri, Some("bob"), body, false).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "bob: {method} {uri}");
    }
    let (s, _, _) = a.call("GET", &m, None, vec![], false).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = a.call("GET", &m, Some("alice"), vec![], false).await;
    assert_eq!(s, StatusCode::OK, "alice's media is untouched");
}

#[tokio::test]
async fn files_dedupe_per_user_and_are_cleaned_up() {
    let a = app("gc");
    let c1 = a.new_card("alice", "one", false).await.1["id"].as_i64().unwrap();
    let c2 = a.new_card("alice", "two", false).await.1["id"].as_i64().unwrap();
    let cb = a.new_card("bob", "bob", false).await.1["id"].as_i64().unwrap();
    let a1 = a.attach("alice", c1, jpeg(1), "a.jpg").await.1["id"].as_i64().unwrap();
    a.attach("alice", c2, jpeg(1), "same.jpg").await;
    a.attach("bob", cb, jpeg(1), "same.jpg").await;
    assert_eq!(a.files(), 2, "same file twice for alice is stored once; bob gets his own copy");

    // removing one of alice's two uses keeps the file; removing the last use deletes it
    let (s, _, _) = a.call("DELETE", &format!("/api/media/{a1}"), Some("alice"), vec![], false).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(a.files(), 2);
    let (s, _, _) = a.call("DELETE", &format!("/api/notes/{c2}"), Some("alice"), vec![], false).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(a.files(), 1, "only bob's copy remains");

    // a media card may lose its text, a plain card may not
    let c3 = a.new_card("alice", "caption", false).await.1["id"].as_i64().unwrap();
    let put = |id: i64| (format!("/api/notes/{id}"), serde_json::json!({ "body": "" }).to_string().into_bytes());
    let (u, b) = put(c3);
    assert_eq!(a.call("PUT", &u, Some("alice"), b, true).await.0, StatusCode::BAD_REQUEST);
    a.attach("alice", c3, jpeg(3), "p.jpg").await;
    let (u, b) = put(c3);
    assert_eq!(a.call("PUT", &u, Some("alice"), b, true).await.0, StatusCode::OK);
    std::fs::remove_dir_all(&a.media).ok();
}
