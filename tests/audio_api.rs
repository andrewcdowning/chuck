//! HTTP-level tests for audio attachments: upload with a duration, tag stripping, byte ranges, isolation.

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
    let media = std::env::temp_dir().join(format!("chuck-audio-test-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&media).ok();
    let state = api::AppState::new(Arc::new(SqliteStore::open(":memory:").unwrap()), Arc::new(HeaderUser), Arc::new(FsBlobStore::open(&media).unwrap()));
    App { router: api::router(state), media }
}

impl App {
    async fn req(&self, method: &str, uri: &str, user: Option<&str>, headers: &[(&str, &str)], body: Vec<u8>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut r = Request::builder().method(method).uri(uri);
        if let Some(u) = user {
            r = r.header("x-user", u);
        }
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        let resp = self.router.clone().oneshot(r.body(Body::from(body)).unwrap()).await.unwrap();
        let (parts, b) = resp.into_parts();
        (parts.status, parts.headers, b.collect().await.unwrap().to_bytes().to_vec())
    }
}

/// A WAV with a LIST/INFO tag naming the artist, and 4000 bytes of audio counting 0..=255 repeatedly.
fn wav() -> Vec<u8> {
    let mut body = b"WAVE".to_vec();
    let mut chunk = |id: &[u8; 4], d: &[u8]| {
        body.extend_from_slice(id);
        body.extend_from_slice(&(d.len() as u32).to_le_bytes());
        body.extend_from_slice(d);
    };
    chunk(b"fmt ", &[1, 0, 1, 0, 0x40, 0x1F, 0, 0, 0x40, 0x1F, 0, 0, 1, 0, 8, 0]);
    chunk(b"LIST", b"INFOIART\x06\0\0\0Alice\0");
    let pcm: Vec<u8> = (0..4000u32).map(|i| (i % 256) as u8).collect();
    chunk(b"data", &pcm);
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend(body);
    v
}

#[tokio::test]
async fn audio_upload_ranges_and_isolation() {
    let a = app("ranges");
    let (_, _, b) = a.req("POST", "/api/notes?media=true", Some("alice"), &[("content-type", "application/json")], br#"{"body":""}"#.to_vec()).await;
    let note: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let nid = note["id"].as_str().unwrap().to_string();

    // bad durations are refused
    for d in ["-1", "999999999"] {
        let (s, _, _) = a.req("POST", &format!("/api/notes/{nid}/attachments?name=v.wav&duration_ms={d}"), Some("alice"), &[], wav()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "duration {d}");
    }
    let (s, _, b) = a.req("POST", &format!("/api/notes/{nid}/attachments?name=voice.wav&duration_ms=500"), Some("alice"), &[], wav()).await;
    assert_eq!(s, StatusCode::CREATED);
    let att: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!((att["mime"].as_str(), att["duration_ms"].as_i64(), att["width"].as_i64()), (Some("audio/wav"), Some(500), None));
    let (id, v) = (att["id"].as_str().unwrap().to_string(), att["v"].as_str().unwrap().to_string());
    let url = format!("/api/media/{id}?v={v}");

    // whole file: tag stripped, ranges advertised
    let (s, h, full) = a.req("GET", &url, Some("alice"), &[], vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["content-type"], "audio/wav");
    assert_eq!(h["accept-ranges"], "bytes");
    assert!(!full.windows(5).any(|w| w == b"Alice"), "artist tag must be stripped");
    let etag = h["etag"].to_str().unwrap().to_string();

    // a range gives exactly those bytes
    let (s, h, part) = a.req("GET", &url, Some("alice"), &[("range", "bytes=100-199")], vec![]).await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);
    assert_eq!(h["content-range"], format!("bytes 100-199/{}", full.len()));
    assert_eq!(part, full[100..200]);
    let (s, _, tail) = a.req("GET", &url, Some("alice"), &[("range", "bytes=-10")], vec![]).await;
    assert_eq!((s, tail.as_slice()), (StatusCode::PARTIAL_CONTENT, &full[full.len() - 10..]));
    // out of range
    let (s, h, _) = a.req("GET", &url, Some("alice"), &[("range", &format!("bytes={}-", full.len()))], vec![]).await;
    assert_eq!(s, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(h["content-range"], format!("bytes */{}", full.len()));
    // If-Range with a stale tag: the whole current file, not a slice of it
    let (s, _, b) = a.req("GET", &url, Some("alice"), &[("range", "bytes=0-9"), ("if-range", "\"stale\"")], vec![]).await;
    assert_eq!((s, b.len()), (StatusCode::OK, full.len()));
    let (s, _, _) = a.req("GET", &url, Some("alice"), &[("range", "bytes=0-9"), ("if-range", &etag)], vec![]).await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);

    // another user gets nothing, ranges included
    let (s, _, _) = a.req("GET", &url, Some("bob"), &[("range", "bytes=0-9")], vec![]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = a.req("GET", &url, None, &[], vec![]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // the card is a media card listing the audio
    let (_, _, b) = a.req("GET", "/api/notes", Some("alice"), &[], vec![]).await;
    let list: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let list = &list["notes"];
    assert_eq!(list[0]["kind"], "media");
    assert_eq!(list[0]["attachments"][0]["mime"], "audio/wav");
    std::fs::remove_dir_all(&a.media).ok();
}
