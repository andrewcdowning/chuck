//! The Smithy model in smithy/model is the written contract of the HTTP API. This test keeps it honest without needing
//! the Smithy tools: every operation in the model must exist on the router with that method, and the router must not
//! answer any method on a model path that the model does not list. (Shapes and status codes are not checked here.)

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chuck::auth::Authenticator;
use chuck::{api, blob::FsBlobStore, sqlite::SqliteStore};
use http_body_util::BodyExt;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tower::ServiceExt;

struct HeaderUser;
impl Authenticator for HeaderUser {
    fn authenticate(&self, h: &HeaderMap) -> Option<String> {
        h.get("x-user")?.to_str().ok().map(str::to_string)
    }
}

/// (uri template, methods) for every `@http(method: "X", uri: "/y")` in the model.
fn model_operations() -> BTreeMap<String, BTreeSet<String>> {
    let src = include_str!("../smithy/model/chuck.smithy");
    let mut ops: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in src.lines().filter(|l| l.trim_start().starts_with("@http(")) {
        let quoted: Vec<&str> = line.split('"').collect();
        // @http(method: "GET", uri: "/api/notes", ...) splits into [.., "GET", .., "/api/notes", ..]
        assert!(quoted.len() >= 4, "cannot read {line}");
        ops.entry(quoted[3].to_string()).or_default().insert(quoted[1].to_string());
    }
    ops
}

#[tokio::test]
async fn router_matches_the_smithy_model() {
    let media = std::env::temp_dir().join(format!("chuck-contract-{}", std::process::id()));
    let state = api::AppState::new(Arc::new(SqliteStore::open(":memory:").unwrap()), Arc::new(HeaderUser), Arc::new(FsBlobStore::open(&media).unwrap())).with_admins(["contract"]); // admin routes are refused to everyone else before routing
    let router = api::router(state);
    let ops = model_operations();
    assert!(ops.len() >= 15, "found only {} paths in the model", ops.len());

    // Paths: the router's API routes and the model's must be the same set.
    let src = include_str!("../src/api.rs");
    let routes: BTreeSet<String> = src
        .split(".route(")
        .skip(1)
        .filter_map(|r| r.trim_start().strip_prefix('"'))
        .map(|r| r.split('"').next().unwrap().to_string())
        .filter(|r| r.starts_with("/api/"))
        .collect();
    let model_paths: BTreeSet<String> = ops.keys().map(|p| p.replace('{', ":").replace('}', "")).collect();
    assert_eq!(routes, model_paths, "src/api.rs routes and smithy/model/chuck.smithy paths differ");

    // Methods: signed in, a method the router does not serve answers 405; one it serves never does.
    for (path, methods) in &ops {
        let uri = path.replace("{id}", "1");
        for m in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
            let req = Request::builder().method(m).uri(&uri).header("x-user", "contract").header("content-type", "application/json").body(Body::empty()).unwrap();
            let resp = router.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let served = status != StatusCode::METHOD_NOT_ALLOWED;
            assert_eq!(served, methods.contains(m), "{m} {path}: model says {}, router answered {status} {}", methods.contains(m), String::from_utf8_lossy(&body));
        }
    }
    std::fs::remove_dir_all(&media).ok();
}
