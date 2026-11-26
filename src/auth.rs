//! Identity extension point. Local mode: everyone is the single user "local",
//! and the server binds to loopback only. For an AWS deployment, add another
//! `Authenticator` (e.g. verify the ALB/Cognito OIDC JWT header and return the
//! `sub`/email claim) and select it in `main.rs`; the store already scopes by owner.

use axum::http::HeaderMap;

pub trait Authenticator: Send + Sync + 'static {
    /// Returns the owner id for the request, or None to reject with 401.
    fn authenticate(&self, headers: &HeaderMap) -> Option<String>;
}

pub struct LocalUser;

impl Authenticator for LocalUser {
    fn authenticate(&self, _headers: &HeaderMap) -> Option<String> {
        Some("local".to_string())
    }
}

/// Optional shared-secret mode: set CHUCK_TOKEN and send `Authorization: Bearer <token>`.
pub struct StaticToken(pub String);

impl Authenticator for StaticToken {
    fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        let v = headers.get("authorization")?.to_str().ok()?;
        (v.strip_prefix("Bearer ")? == self.0).then(|| "local".to_string())
    }
}
