//! How to plug in your own authentication from a separate binary. Run with:
//!
//!   CHUCK_AUTH=corp CHUCK_CORP_USER_HEADER=x-forwarded-user cargo run --example custom_auth
//!
//! In your own repo, add `chuck = { git = "https://github.com/andrewcdowning/chuck" }` and copy this file to `src/main.rs`.
//! Replace `CorpSso::authenticate` with whatever your edge layer provides.

use axum::http::HeaderMap;
use chuck::auth::{sanitize_owner, Authenticator};
use std::sync::Arc;

struct CorpSso {
    user_header: String,
}

impl Authenticator for CorpSso {
    fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        // Verify or decode whatever your SSO layer sends, then return the user's id.
        // Return None to reject with 401. Always pass the result through `sanitize_owner`.
        let v = headers.get(self.user_header.as_str())?.to_str().ok()?;
        sanitize_owner(v)
    }
}

#[tokio::main]
async fn main() {
    let opts = chuck::Options::from_env().with_provider("corp", || {
        let user_header = std::env::var("CHUCK_CORP_USER_HEADER").map_err(|_| "set CHUCK_CORP_USER_HEADER".to_string())?;
        Ok(Arc::new(CorpSso { user_header }) as Arc<dyn Authenticator>)
    });
    if let Err(e) = chuck::run(opts).await {
        eprintln!("{e}");
        std::process::exit(2);
    }
}
