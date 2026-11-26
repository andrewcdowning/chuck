mod api;
mod auth;
mod sqlite;
mod store;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Config via env so the same binary runs locally or in a container.
    let db = std::env::var("CHUCK_DB").unwrap_or_else(|_| "chuck.db".into());
    // Default is loopback only. Set CHUCK_ADDR=0.0.0.0:8080 only behind auth (e.g. ALB + OIDC).
    let addr = std::env::var("CHUCK_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());

    let store = sqlite::SqliteStore::open(&db).expect("open database");
    let cfg = match auth::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("auth config error: {e}");
            std::process::exit(2);
        }
    };

    // Refuse to serve unauthenticated on a non-loopback address unless the operator says so explicitly.
    let loopback = addr.parse::<std::net::SocketAddr>().map(|a| a.ip().is_loopback()).unwrap_or(false);
    if cfg.is_local_only && !loopback {
        if std::env::var("CHUCK_ALLOW_INSECURE").as_deref() == Ok("1") {
            eprintln!("WARNING: serving without authentication on {addr} (CHUCK_ALLOW_INSECURE=1)");
        } else {
            eprintln!("refusing to listen on {addr} without authentication: set CHUCK_AUTH (token, header, jwt) or CHUCK_ALLOW_INSECURE=1");
            std::process::exit(2);
        }
    }

    let app = api::router(api::AppState { store: Arc::new(store), auth: cfg.auth });
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("chuck listening on http://{addr} (db: {db}, auth: {})", cfg.names.join(","));
    axum::serve(listener, app).await.unwrap();
}
