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
    let auth: Arc<dyn auth::Authenticator> = match std::env::var("CHUCK_TOKEN") {
        Ok(t) if !t.is_empty() => Arc::new(auth::StaticToken(t)),
        _ => Arc::new(auth::LocalUser),
    };

    let app = api::router(api::AppState { store: Arc::new(store), auth });
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("chuck listening on http://{addr} (db: {db})");
    axum::serve(listener, app).await.unwrap();
}
