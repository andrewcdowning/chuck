//! Data-boundary tests. Alice owns notes, a todo card, a reminder, pictures, audio, a rule and feedback, all carrying a
//! marker string. Every endpoint is then tried as another user and as an administrator, and none of them may read,
//! change, delete, search, export or get suggestions from her data. The last test fails if a route is added to the API
//! without being listed here, so a new endpoint cannot ship without its isolation test.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chuck::auth::{sanitize_name, Authenticator, Identity};
use chuck::{api, blob::FsBlobStore, sqlite::SqliteStore};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const MARK: &str = "ALICE_SECRET_7Q";

/// Test identity: `x-user` is the account, `x-name` an optional display name.
struct HeaderUser;
impl Authenticator for HeaderUser {
    fn authenticate(&self, h: &HeaderMap) -> Option<String> {
        h.get("x-user")?.to_str().ok().map(str::to_string)
    }
    fn identity(&self, h: &HeaderMap) -> Option<Identity> {
        let id = self.authenticate(h)?;
        Some(Identity { id, name: h.get("x-name").and_then(|v| v.to_str().ok()).and_then(sanitize_name) })
    }
}

struct App {
    router: axum::Router,
    media: PathBuf,
}

fn app(name: &str) -> App {
    let media = std::env::temp_dir().join(format!("chuck-isolation-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&media).ok();
    let mut state = api::AppState::new(Arc::new(SqliteStore::open(":memory:").unwrap()), Arc::new(HeaderUser), Arc::new(FsBlobStore::open(&media).unwrap()));
    state.admins = Arc::new(HashSet::from(["root".to_string()]));
    App { router: api::router(state), media }
}

type Reply = (StatusCode, HeaderMap, Vec<u8>);

impl App {
    async fn raw(&self, method: &str, uri: &str, user: Option<&str>, headers: &[(&str, &str)], body: Vec<u8>) -> Reply {
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

    async fn call(&self, method: &str, uri: &str, user: &str, body: Option<Value>) -> Reply {
        match body {
            Some(b) => self.raw(method, uri, Some(user), &[("content-type", "application/json")], b.to_string().into_bytes()).await,
            None => self.raw(method, uri, Some(user), &[], vec![]).await,
        }
    }

    async fn json(&self, method: &str, uri: &str, user: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, _, b) = self.call(method, uri, user, body).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
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

fn jpeg(salt: u8) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8];
    j.extend(seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
    j.extend(seg(0xDB, &[salt; 65]));
    j.extend(seg(0xC0, &[8, 0x01, 0x2C, 0x03, 0x20, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]));
    j.extend(seg(0xDA, &[3, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]));
    j.extend_from_slice(&[0x12, salt, 0xFF, 0xD9]);
    j
}

fn wav() -> Vec<u8> {
    let mut body = b"WAVE".to_vec();
    let mut chunk = |id: &[u8; 4], d: &[u8]| {
        body.extend_from_slice(id);
        body.extend_from_slice(&(d.len() as u32).to_le_bytes());
        body.extend_from_slice(d);
    };
    chunk(b"fmt ", &[1, 0, 1, 0, 0x40, 0x1F, 0, 0, 0x40, 0x1F, 0, 0, 1, 0, 8, 0]);
    chunk(b"data", &[7u8; 400]);
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend(body);
    v
}

struct Fixture {
    note: i64,
    todo: i64,
    reminder: i64,
    image: i64,
    audio: i64,
    rule: i64,
}

/// Alice's data: every kind of thing a user can own, each carrying the marker.
async fn alice_data(a: &App) -> Fixture {
    let make = |body: Value| async move { body };
    let _ = make;
    let note = a.json("POST", "/api/notes", "alice", Some(json!({ "body": format!("{MARK} plan #alicetag") }))).await.1["id"].as_i64().unwrap();
    let todo = a.json("POST", "/api/notes", "alice", Some(json!({ "body": format!("{MARK} chores\n- [ ] sweep"), "due_at": 4_102_444_800i64 }))).await.1["id"].as_i64().unwrap();
    let reminder = a.json("POST", "/api/notes", "alice", Some(json!({ "body": format!("{MARK} call"), "remind_at": 4_102_444_800i64 }))).await.1["id"].as_i64().unwrap();
    let image = a.raw("POST", &format!("/api/notes/{note}/attachments?name={MARK}.jpg"), Some("alice"), &[], jpeg(1)).await;
    assert_eq!(image.0, StatusCode::CREATED);
    let image = serde_json::from_slice::<Value>(&image.2).unwrap()["id"].as_i64().unwrap();
    let audio = a.raw("POST", &format!("/api/notes/{note}/attachments?name={MARK}.wav&duration_ms=100"), Some("alice"), &[], wav()).await;
    assert_eq!(audio.0, StatusCode::CREATED);
    let audio = serde_json::from_slice::<Value>(&audio.2).unwrap()["id"].as_i64().unwrap();
    let rule = a.json("POST", "/api/rules", "alice", Some(json!({ "pattern": MARK, "tag": "alicetag" }))).await.1["id"].as_i64().unwrap();
    let (s, _, _) = a.call("POST", "/api/suggest/feedback", "alice", Some(json!({ "tag": "alicetag", "action": "accepted", "confidence": 0.9 }))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    Fixture { note, todo, reminder, image, audio, rule }
}

fn text(r: &Reply) -> String {
    String::from_utf8_lossy(&r.2).to_string()
}

fn no_mark(r: &Reply, what: &str) {
    assert!(!text(r).contains(MARK), "{what} leaked Alice's data: {}", &text(r)[..text(r).len().min(300)]);
    // the marker could also be hidden in a binary: check the raw bytes too
    assert!(!r.2.windows(MARK.len()).any(|w| w == MARK.as_bytes()), "{what} leaked Alice's data (bytes)");
}

/// Every endpoint, as `user`, aimed at Alice's data. Nothing may be read, changed or deleted.
async fn attack(a: &App, user: &str, f: &Fixture) {
    // reading and searching
    for q in ["", &format!("?q={MARK}"), "?tag=alicetag", "?reminders=true", "?limit=200", &format!("?q={MARK}&tag=alicetag&limit=200")] {
        let r = a.call("GET", &format!("/api/notes{q}"), user, None).await;
        assert_eq!(r.0, StatusCode::OK);
        no_mark(&r, &format!("{user} GET /api/notes{q}"));
        assert_eq!(serde_json::from_slice::<Value>(&r.2).unwrap()["notes"].as_array().unwrap().len(), 0, "{user} saw cards with {q}");
    }
    let r = a.call("GET", "/api/tags", user, None).await;
    assert_eq!(text(&r), r#"{"tags":[]}"#, "{user} saw Alice's tags");
    for q in ["", &format!("?q={MARK}"), "?tag=alicetag", "?reminders=true"] {
        let r = a.call("GET", &format!("/api/export{q}"), user, None).await;
        no_mark(&r, &format!("{user} GET /api/export{q}"));
        assert!(text(&r).contains("0 notes"), "{user} exported cards with {q}");
    }
    // suggestions
    let r = a.call("POST", "/api/suggest", user, Some(json!({ "text": format!("{MARK} plan") }))).await;
    no_mark(&r, "suggest");
    let v: Value = serde_json::from_slice(&r.2).unwrap();
    assert_eq!((v["tagged_notes"].as_i64(), v["suggestions"].as_array().map(Vec::len)), (Some(0), Some(0)), "{user} got suggestions from Alice's tags");
    // rules
    assert_eq!(text(&a.call("GET", "/api/rules", user, None).await), r#"{"rules":[]}"#);
    assert_eq!(a.json("POST", "/api/rules/apply", user, None).await.1["changed"], 0);
    assert_eq!(a.call("DELETE", &format!("/api/rules/{}", f.rule), user, None).await.0, StatusCode::NOT_FOUND);
    // changing and deleting her cards
    for id in [f.note, f.todo, f.reminder] {
        assert_eq!(a.call("PUT", &format!("/api/notes/{id}"), user, Some(json!({ "body": "hacked" }))).await.0, StatusCode::NOT_FOUND, "{user} PUT {id}");
        assert_eq!(a.call("DELETE", &format!("/api/notes/{id}"), user, None).await.0, StatusCode::NOT_FOUND, "{user} DELETE {id}");
        assert_eq!(a.call("POST", &format!("/api/notes/{id}/move"), user, Some(json!({ "prev": null, "next": 999_999 }))).await.0, StatusCode::NOT_FOUND, "{user} move {id}");
        assert_eq!(a.raw("POST", &format!("/api/notes/{id}/attachments"), Some(user), &[], jpeg(9)).await.0, StatusCode::NOT_FOUND, "{user} attach to {id}");
    }
    // her files
    for id in [f.image, f.audio] {
        for uri in [format!("/api/media/{id}"), format!("/api/media/{id}?thumb=true"), format!("/api/media/{id}?v=anything")] {
            assert_eq!(a.call("GET", &uri, user, None).await.0, StatusCode::NOT_FOUND, "{user} GET {uri}");
        }
        let ranged = a.raw("GET", &format!("/api/media/{id}"), Some(user), &[("range", "bytes=0-9")], vec![]).await;
        assert_eq!(ranged.0, StatusCode::NOT_FOUND, "{user} ranged read of {id}");
        assert_eq!(a.call("DELETE", &format!("/api/media/{id}"), user, None).await.0, StatusCode::NOT_FOUND, "{user} DELETE media {id}");
        assert_eq!(a.raw("PUT", &format!("/api/media/{id}/thumb"), Some(user), &[], jpeg(9)).await.0, StatusCode::NOT_FOUND, "{user} thumb {id}");
    }
    // their own account endpoints show only themselves
    let r = a.call("GET", "/api/me", user, None).await;
    assert_eq!(serde_json::from_slice::<Value>(&r.2).unwrap()["id"], user);
    no_mark(&r, "me");
    let r = a.call("GET", "/api/me/export", user, None).await;
    assert_eq!(r.0, StatusCode::OK);
    no_mark(&r, &format!("{user} data export (zip)"));
    let names: Vec<String> = chuck::zip::read_stored(&r.2).unwrap().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["README.txt", "notes.json", "notes.md", "rules.json", "suggestion-feedback.json"], "{user}'s export contains other files");
    // feedback only ever lands on the caller's own account
    let (s, _, _) = a.call("POST", "/api/suggest/feedback", user, Some(json!({ "tag": "alicetag", "action": "dismissed", "confidence": 0.9 }))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

async fn alice_is_intact(a: &App, f: &Fixture) {
    let (s, v) = a.json("GET", "/api/notes?limit=200", "alice", None).await;
    assert_eq!(s, StatusCode::OK);
    let notes = v["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 3);
    assert!(notes.iter().all(|n| n["body"].as_str().unwrap().contains(MARK)), "a card was changed");
    let note = notes.iter().find(|n| n["id"] == f.note).unwrap();
    assert_eq!(note["attachments"].as_array().unwrap().len(), 2);
    assert_eq!(a.json("GET", "/api/rules", "alice", None).await.1["rules"].as_array().unwrap().len(), 1);
    for id in [f.image, f.audio] {
        assert_eq!(a.call("GET", &format!("/api/media/{id}"), "alice", None).await.0, StatusCode::OK);
    }
    let r = a.call("GET", "/api/me/export", "alice", None).await;
    let files = chuck::zip::read_stored(&r.2).unwrap();
    let events: Value = serde_json::from_slice(&files.iter().find(|(n, _)| n == "suggestion-feedback.json").unwrap().1).unwrap();
    assert_eq!(events.as_array().unwrap().len(), 1, "other users' feedback must not land on Alice's account");
}

#[tokio::test]
async fn other_users_and_admins_cannot_touch_alices_data() {
    let a = app("attack");
    let f = alice_data(&a).await;
    for user in ["bob", "root"] {
        attack(&a, user, &f).await;
    }
    alice_is_intact(&a, &f).await;
    std::fs::remove_dir_all(&a.media).ok();
}

#[tokio::test]
async fn admins_see_accounts_but_never_content() {
    let a = app("admin");
    let f = alice_data(&a).await;
    a.json("GET", "/api/me", "bob", None).await;

    // non-admins cannot use the admin endpoints at all
    assert_eq!(a.call("GET", "/api/admin/users", "bob", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(a.call("POST", "/api/admin/users/status", "bob", Some(json!({ "id": "alice", "active": false }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(a.call("POST", "/api/admin/users/delete", "bob", Some(json!({ "id": "alice" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(a.json("GET", "/api/me", "alice", None).await.1["status"], "active", "alice still active after bob's attempts");

    // the account list carries counts and sizes, and nothing else
    let r = a.call("GET", "/api/admin/users", "root", None).await;
    no_mark(&r, "admin user list");
    let list: Vec<Value> = serde_json::from_slice::<Value>(&r.2).unwrap()["users"].as_array().unwrap().clone();
    let alice = list.iter().find(|u| u["id"] == "alice").unwrap();
    assert_eq!((alice["notes"].as_i64(), alice["attachments"].as_i64()), (Some(3), Some(2)));
    let mut keys: Vec<&str> = alice.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, vec!["attachments", "created_at", "display_name", "id", "last_seen_at", "notes", "role", "status", "storage_bytes"]);
    assert_eq!(list.iter().find(|u| u["id"] == "root").unwrap()["role"], "admin");
    assert_eq!(alice["role"], "user");

    // an admin cannot lock themselves out or delete themselves through the admin screen
    assert_eq!(a.call("POST", "/api/admin/users/status", "root", Some(json!({ "id": "root", "active": false }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(a.call("POST", "/api/admin/users/delete", "root", Some(json!({ "id": "root" }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(a.call("POST", "/api/admin/users/status", "root", Some(json!({ "id": "nobody", "active": false }))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.call("POST", "/api/admin/users/delete", "root", Some(json!({ "id": "nobody" }))).await.0, StatusCode::NOT_FOUND);

    // disabling shuts an account out of every endpoint, others unaffected, nothing is lost
    let (s, v) = a.json("POST", "/api/admin/users/status", "root", Some(json!({ "id": "alice", "active": false }))).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("disabled")));
    for (method, uri) in [
        ("GET", "/api/notes".to_string()),
        ("GET", "/api/tags".to_string()),
        ("GET", "/api/me".to_string()),
        ("GET", "/api/me/export".to_string()),
        ("GET", format!("/api/media/{}", f.image)),
        ("POST", "/api/notes".to_string()),
        ("DELETE", format!("/api/notes/{}", f.note)),
    ] {
        let r = a.call(method, &uri, "alice", Some(json!({ "body": "x" }))).await;
        assert_eq!(r.0, StatusCode::UNAUTHORIZED, "disabled alice: {method} {uri}");
    }
    assert_eq!(a.call("GET", "/api/notes", "bob", None).await.0, StatusCode::OK);
    a.json("POST", "/api/admin/users/status", "root", Some(json!({ "id": "alice", "active": true }))).await;
    alice_is_intact(&a, &f).await;

    // deleting an account removes its data and its files, and nothing of anyone else's
    let bob_card = a.json("POST", "/api/notes", "bob", Some(json!({ "body": "bob's card" }))).await.1["id"].as_i64().unwrap();
    a.raw("POST", &format!("/api/notes/{bob_card}/attachments"), Some("bob"), &[], jpeg(5)).await;
    assert_eq!(a.files(), 3, "alice's picture and audio, bob's picture");
    assert_eq!(a.call("POST", "/api/admin/users/delete", "root", Some(json!({ "id": "alice" }))).await.0, StatusCode::NO_CONTENT);
    assert_eq!(a.files(), 1, "only bob's file remains");
    assert_eq!(a.json("GET", "/api/notes", "alice", None).await.1["notes"].as_array().unwrap().len(), 0, "signing in again starts an empty account");
    assert_eq!(a.json("GET", "/api/notes", "bob", None).await.1["notes"].as_array().unwrap().len(), 1);
    let (_, list) = a.json("GET", "/api/admin/users", "root", None).await;
    assert_eq!(list["users"].as_array().unwrap().iter().find(|u| u["id"] == "alice").unwrap()["notes"], 0);
    std::fs::remove_dir_all(&a.media).ok();
}

#[tokio::test]
async fn my_data_export_and_account_deletion() {
    let a = app("mine");
    let f = alice_data(&a).await;
    a.json("POST", "/api/notes", "bob", Some(json!({ "body": "bob's own" }))).await;

    // Alice's export holds everything she owns, including the original files
    let r = a.call("GET", "/api/me/export", "alice", None).await;
    assert_eq!(r.0, StatusCode::OK);
    assert_eq!(r.1["content-type"], "application/zip");
    assert!(r.1["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"chuck-data-"));
    let files = chuck::zip::read_stored(&r.2).unwrap();
    let get = |n: &str| files.iter().find(|(name, _)| name == n).map(|(_, b)| b.clone()).unwrap_or_else(|| panic!("{n} missing; have {:?}", files.iter().map(|f| &f.0).collect::<Vec<_>>()));
    let notes: Value = serde_json::from_slice(&get("notes.json")).unwrap();
    assert_eq!(notes.as_array().unwrap().len(), 3);
    assert!(String::from_utf8(get("notes.md")).unwrap().contains(MARK));
    assert_eq!(serde_json::from_slice::<Value>(&get("rules.json")).unwrap().as_array().unwrap().len(), 1);
    let image_in_zip = get(&format!("attachments/{}-{MARK}.jpg", f.image));
    let (_, _, image_served) = a.call("GET", &format!("/api/media/{}", f.image), "alice", None).await;
    assert_eq!(image_in_zip, image_served, "the export holds the same file the app serves");
    assert!(files.iter().any(|(n, _)| n == &format!("attachments/{}-{MARK}.wav", f.audio)));
    assert!(!files.iter().any(|(_, b)| b.windows(8).any(|w| w == b"bob's ow")), "bob's data is not in Alice's export");

    // display names come from the identity, are sanitized, and are not required
    let (_, _, b) = a.raw("GET", "/api/me", Some("carol"), &[("x-name", "Carol C")], vec![]).await;
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["display_name"], "Carol C");
    let long = "x".repeat(200);
    let (_, _, b) = a.raw("GET", "/api/me", Some("dave"), &[("x-name", &long)], vec![]).await;
    assert!(serde_json::from_slice::<Value>(&b).unwrap()["display_name"].is_null());
    let (_, _, b) = a.raw("GET", "/api/me", Some("carol"), &[], vec![]).await;
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["display_name"], "Carol C", "a request without a name keeps the known one");

    // deleting your own account needs the account name typed back
    assert_eq!(a.call("DELETE", "/api/me", "alice", Some(json!({ "confirm": "bob" }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(a.call("DELETE", "/api/me", "alice", Some(json!({ "confirm": "" }))).await.0, StatusCode::BAD_REQUEST);
    alice_is_intact(&a, &f).await;
    assert_eq!(a.call("DELETE", "/api/me", "alice", Some(json!({ "confirm": "alice" }))).await.0, StatusCode::NO_CONTENT);
    assert_eq!(a.files(), 0, "her files are gone");
    assert_eq!(a.json("GET", "/api/notes", "alice", None).await.1["notes"].as_array().unwrap().len(), 0);
    assert_eq!(a.json("GET", "/api/notes", "bob", None).await.1["notes"].as_array().unwrap().len(), 1, "bob untouched");
    // with no identity there is no account to delete
    assert_eq!(a.raw("DELETE", "/api/me", None, &[("content-type", "application/json")], br#"{"confirm":"x"}"#.to_vec()).await.0, StatusCode::UNAUTHORIZED);
    std::fs::remove_dir_all(&a.media).ok();
}

#[tokio::test]
async fn unauthenticated_requests_are_refused_everywhere() {
    let a = app("anon");
    let f = alice_data(&a).await;
    let m = f.image;
    for (method, uri) in [
        ("GET", "/api/notes".to_string()),
        ("POST", "/api/notes".to_string()),
        ("PUT", format!("/api/notes/{}", f.note)),
        ("DELETE", format!("/api/notes/{}", f.note)),
        ("POST", format!("/api/notes/{}/move", f.note)),
        ("POST", format!("/api/notes/{}/attachments", f.note)),
        ("GET", "/api/tags".to_string()),
        ("GET", "/api/export".to_string()),
        ("POST", "/api/suggest".to_string()),
        ("POST", "/api/suggest/feedback".to_string()),
        ("GET", "/api/rules".to_string()),
        ("POST", "/api/rules".to_string()),
        ("POST", "/api/rules/apply".to_string()),
        ("DELETE", format!("/api/rules/{}", f.rule)),
        ("GET", format!("/api/media/{m}")),
        ("DELETE", format!("/api/media/{m}")),
        ("PUT", format!("/api/media/{m}/thumb")),
        ("GET", "/api/me".to_string()),
        ("DELETE", "/api/me".to_string()),
        ("GET", "/api/me/export".to_string()),
        ("GET", "/api/admin/users".to_string()),
        ("POST", "/api/admin/users/status".to_string()),
        ("POST", "/api/admin/users/delete".to_string()),
    ] {
        let r = a.raw(method, &uri, None, &[("content-type", "application/json")], b"{}".to_vec()).await;
        assert_eq!(r.0, StatusCode::UNAUTHORIZED, "{method} {uri} answered without any identity");
    }
    // only the page itself and the health check are public, and neither carries data
    let (s, _, page) = a.raw("GET", "/", None, &[], vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&page).contains(MARK));
    assert_eq!(a.raw("GET", "/healthz", None, &[], vec![]).await.0, StatusCode::OK);
    std::fs::remove_dir_all(&a.media).ok();
}

/// Every route in the API must appear here, next to the test that covers it. Adding a route without adding it to this
/// list (and to the tests above) fails the build, so no endpoint can be added without its isolation check.
#[test]
fn every_route_has_an_isolation_test() {
    const COVERED: &[&str] = &[
        "/",
        "/healthz",
        "/api/notes",
        "/api/notes/:id",
        "/api/notes/:id/move",
        "/api/notes/:id/attachments",
        "/api/tags",
        "/api/export",
        "/api/suggest",
        "/api/suggest/feedback",
        "/api/rules",
        "/api/rules/apply",
        "/api/rules/:id",
        "/api/media/:id",
        "/api/media/:id/thumb",
        "/api/me",
        "/api/me/export",
        "/api/admin/users",
        "/api/admin/users/status",
        "/api/admin/users/delete",
    ];
    let src = include_str!("../src/api.rs");
    let routes: Vec<&str> = src.split(".route(").skip(1).filter_map(|r| r.trim_start().strip_prefix('"')).map(|r| r.split('"').next().unwrap()).collect();
    assert!(routes.len() >= COVERED.len() - 2, "found only {} routes, is the router still in src/api.rs?", routes.len());
    for r in &routes {
        assert!(COVERED.contains(r), "route {r} has no isolation test: add it to tests/isolation.rs");
    }
    for c in COVERED {
        assert!(routes.contains(c), "{c} is listed in tests/isolation.rs but is not a route any more");
    }
}
