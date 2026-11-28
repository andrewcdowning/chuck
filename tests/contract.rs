//! The Smithy model in smithy/model is the written contract of the HTTP API, one service per plane (`ChuckData`,
//! `ChuckControl`, `ChuckAdmin`). This test keeps it honest without needing the Smithy tools: for each service, the
//! operations in the model must be exactly the routes of the matching router function in `src/api.rs`, with the same
//! HTTP methods. It also checks that every operation belongs to exactly one service, and that a router built for some
//! planes serves only those. (Shapes and status codes are not checked here.)

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chuck::api::{self, Plane};
use chuck::auth::Authenticator;
use chuck::{blob::FsBlobStore, sqlite::SqliteStore};
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

const MODEL_FILES: [&str; 4] = [
    include_str!("../smithy/model/common.smithy"),
    include_str!("../smithy/model/data.smithy"),
    include_str!("../smithy/model/control.smithy"),
    include_str!("../smithy/model/admin.smithy"),
];

/// service name -> router function in src/api.rs
const PLANES: [(&str, &str); 3] = [("ChuckData", "data_routes"), ("ChuckControl", "control_routes"), ("ChuckAdmin", "admin_routes")];

#[derive(Default)]
struct Model {
    /// service -> its operations
    services: BTreeMap<String, BTreeSet<String>>,
    /// operation -> (method, uri)
    http: BTreeMap<String, (String, String)>,
}

fn parse_model() -> Model {
    let mut m = Model::default();
    for src in MODEL_FILES {
        let (mut last_http, mut service, mut in_ops) = (None::<(String, String)>, None::<String>, false);
        for line in src.lines().map(str::trim) {
            if line.starts_with("@http(") {
                // @http(method: "GET", uri: "/api/notes", ...) splits into [.., "GET", .., "/api/notes", ..]
                let q: Vec<&str> = line.split('"').collect();
                assert!(q.len() >= 4, "cannot read {line}");
                last_http = Some((q[1].to_string(), q[3].to_string()));
            } else if let Some(rest) = line.strip_prefix("operation ") {
                let name = rest.split_whitespace().next().unwrap().to_string();
                let http = last_http.take().unwrap_or_else(|| panic!("operation {name} has no @http trait"));
                m.http.insert(name, http);
            } else if let Some(rest) = line.strip_prefix("service ") {
                service = Some(rest.split_whitespace().next().unwrap().to_string());
            } else if line.starts_with("operations: [") {
                in_ops = true;
            } else if in_ops {
                if line == "]" {
                    in_ops = false;
                    service = None;
                } else if !line.is_empty() {
                    m.services.entry(service.clone().expect("operations outside a service")).or_default().insert(line.to_string());
                }
            }
        }
    }
    m
}

/// The `.route("...")` paths inside one function of src/api.rs.
fn routes_in(fn_name: &str) -> BTreeSet<String> {
    let src = include_str!("../crates/chuck-http/src/api.rs");
    let start = src.find(&format!("pub fn {fn_name}(")).unwrap_or_else(|| panic!("src/api.rs has no {fn_name}"));
    let body = &src[start..];
    let end = body.find("\n}\n").expect("end of function");
    body[..end].split(".route(").skip(1).filter_map(|r| r.trim_start().strip_prefix('"')).map(|r| r.split('"').next().unwrap().to_string()).collect()
}

fn state(media: &std::path::Path) -> api::AppState {
    api::AppState::new(Arc::new(SqliteStore::open(":memory:").unwrap()), Arc::new(HeaderUser), Arc::new(FsBlobStore::open(media).unwrap()))
        .with_admins(["contract"]) // admin routes are refused to everyone else before routing
}

async fn status(router: &axum::Router, method: &str, uri: &str) -> (StatusCode, String) {
    let req = Request::builder().method(method).uri(uri).header("x-user", "contract").header("content-type", "application/json").body(Body::empty()).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let code = resp.status();
    (code, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

#[test]
fn every_service_matches_its_router_function() {
    let m = parse_model();
    assert_eq!(m.services.keys().cloned().collect::<Vec<_>>(), {
        let mut v: Vec<String> = PLANES.iter().map(|(s, _)| s.to_string()).collect();
        v.sort();
        v
    });
    for (service, router_fn) in PLANES {
        let ops = &m.services[service];
        let model_paths: BTreeSet<String> = ops.iter().map(|o| m.http[o].1.replace('{', ":").replace('}', "")).collect();
        assert!(!model_paths.is_empty(), "{service} has no operations");
        assert_eq!(routes_in(router_fn), model_paths, "{router_fn} in src/api.rs and service {service} in smithy/model differ");
    }

    // an operation belongs to exactly one service
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for (service, ops) in &m.services {
        for op in ops {
            assert!(m.http.contains_key(op), "{service} lists {op}, which is not defined");
            assert_eq!(owner.insert(op, service), None, "{op} is in two services");
        }
    }
    for op in m.http.keys() {
        assert!(owner.contains_key(op.as_str()), "operation {op} is not in any service");
    }
}

#[tokio::test]
async fn methods_match_the_model() {
    let media = std::env::temp_dir().join(format!("chuck-contract-{}", std::process::id()));
    let router = api::router(state(&media));
    let m = parse_model();
    let mut by_path: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (method, uri) in m.http.values() {
        by_path.entry(uri.clone()).or_default().insert(method.clone());
    }
    assert!(by_path.len() >= 15, "found only {} paths in the model", by_path.len());
    // Signed in, a method the router does not serve answers 405; one it serves never does.
    for (path, methods) in &by_path {
        let uri = path.replace("{id}", "1");
        for method in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
            let (code, body) = status(&router, method, &uri).await;
            let served = code != StatusCode::METHOD_NOT_ALLOWED;
            assert_eq!(served, methods.contains(method), "{method} {path}: model says {}, router answered {code} {body}", methods.contains(method));
        }
    }
    std::fs::remove_dir_all(&media).ok();
}

#[tokio::test]
async fn a_router_for_some_planes_serves_only_those() {
    let media = std::env::temp_dir().join(format!("chuck-contract-planes-{}", std::process::id()));
    let (data, control, admin) = ("/api/notes", "/api/me", "/api/admin/users");
    let cases = [(Plane::Data, [true, false, false]), (Plane::Control, [false, true, false]), (Plane::Admin, [false, false, true])];
    for (plane, serves) in cases {
        let router = api::router_for(state(&media), &[plane]);
        for (uri, should) in [data, control, admin].into_iter().zip(serves) {
            let (code, body) = status(&router, "GET", uri).await;
            if should {
                assert_eq!(code, StatusCode::OK, "{plane:?} should serve {uri}: {body}");
            } else {
                assert_eq!(code, StatusCode::NOT_FOUND, "{plane:?} should not serve {uri}: {body}");
            }
        }
        assert_eq!(status(&router, "GET", "/healthz").await.0, StatusCode::OK, "{plane:?} still answers health checks");
    }
    // anonymous callers are refused by every plane before routing, so which paths exist is not revealed
    let router = api::router_for(state(&media), &[Plane::Data]);
    let req = Request::builder().method("GET").uri("/api/admin/users").body(Body::empty()).unwrap();
    assert_eq!(router.oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    std::fs::remove_dir_all(&media).ok();
}
