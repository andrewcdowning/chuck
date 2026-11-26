//! Pluggable authentication.
//!
//! An `Authenticator` turns request headers into an owner id (or rejects). Every store call is scoped by that
//! owner. Providers are selected at startup from `CHUCK_AUTH`, a comma-separated list tried in order; the
//! first one that accepts the request wins:
//!
//!   local    single user "local"; only allowed on a loopback address (the default)
//!   token    `Authorization: Bearer $CHUCK_TOKEN` (scripts and CLIs; a browser cannot send it)
//!   header   trust a user header set by a reverse proxy / CDN, guarded by a shared secret header
//!   jwt      verify a signed JWT (OIDC/Cognito/ALB/any IdP) against a JWKS or per-kid key URL
//!
//! To add a provider without touching this crate, register it from your own binary (`Options::with_provider`).
//! See docs/AUTH.md.

mod jwt;
mod simple;

use axum::http::HeaderMap;
use std::sync::Arc;

pub use simple::{ct_eq, LocalUser, StaticToken, TrustedHeader};

pub trait Authenticator: Send + Sync + 'static {
    /// Returns the owner id for the request, or None to reject with 401.
    /// May block briefly (e.g. fetching signing keys); the API calls it on a blocking thread.
    fn authenticate(&self, headers: &HeaderMap) -> Option<String>;
}

/// Tries each provider in order.
pub struct Chain(pub Vec<Arc<dyn Authenticator>>);

impl Authenticator for Chain {
    fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        self.0.iter().find_map(|a| a.authenticate(headers))
    }
}

/// Owner ids come from outside (headers, token claims). Keep them short, printable and non-empty.
pub fn sanitize_owner(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 128 || s.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(s.to_string())
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Builds an authenticator from the environment. Registered by embedding applications under a name.
pub type ProviderFactory = Box<dyn Fn() -> Result<Arc<dyn Authenticator>, String> + Send + Sync>;

/// Extra providers supplied by whoever embeds Chuck (for example a company SSO). They are selected like the
/// built-ins: `CHUCK_AUTH=<name>`, or in a list such as `CHUCK_AUTH=<name>,token`.
#[derive(Default)]
pub struct Providers(std::collections::HashMap<String, ProviderFactory>);

impl Providers {
    pub fn register(&mut self, name: &str, factory: ProviderFactory) -> Result<(), String> {
        let name = name.trim().to_lowercase();
        if BUILTIN.contains(&name.as_str()) {
            return Err(format!("`{name}` is a built-in provider name"));
        }
        if self.0.insert(name.clone(), factory).is_some() {
            return Err(format!("provider `{name}` registered twice"));
        }
        Ok(())
    }
}

const BUILTIN: [&str; 4] = ["local", "token", "header", "jwt"];

pub struct Configured {
    pub auth: Arc<dyn Authenticator>,
    /// True when the only provider is `local` (no real authentication).
    pub is_local_only: bool,
    pub names: Vec<String>,
}

pub fn from_env(extra: &Providers) -> Result<Configured, String> {
    let spec = env("CHUCK_AUTH").unwrap_or_else(|| if env("CHUCK_TOKEN").is_some() { "token".into() } else { "local".into() });
    let names: Vec<String> = spec.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    if names.is_empty() {
        return Err("CHUCK_AUTH is empty".into());
    }
    if names.iter().any(|n| n == "local") && names.len() > 1 {
        return Err("CHUCK_AUTH: `local` accepts everyone and cannot be combined with other providers".into());
    }
    let mut providers: Vec<Arc<dyn Authenticator>> = Vec::new();
    for n in &names {
        let p: Arc<dyn Authenticator> = match n.as_str() {
            "local" => Arc::new(LocalUser),
            "token" => Arc::new(StaticToken::from_env()?),
            "header" => Arc::new(TrustedHeader::from_env()?),
            "jwt" => Arc::new(jwt::JwtAuth::from_env()?),
            other => match extra.0.get(other) {
                Some(factory) => factory().map_err(|e| format!("CHUCK_AUTH={other}: {e}"))?,
                None => {
                    let mut known: Vec<&str> = BUILTIN.to_vec();
                    known.extend(extra.0.keys().map(String::as_str));
                    return Err(format!("CHUCK_AUTH: unknown provider `{other}` (expected one of: {})", known.join(", ")));
                }
            },
        };
        providers.push(p);
    }
    let is_local_only = names == ["local"];
    let auth: Arc<dyn Authenticator> = if providers.len() == 1 { providers.remove(0) } else { Arc::new(Chain(providers)) };
    Ok(Configured { auth, is_local_only, names })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_sanitizing() {
        assert_eq!(sanitize_owner("  alice  ").as_deref(), Some("alice"));
        assert!(sanitize_owner("").is_none());
        assert!(sanitize_owner("a\nb").is_none());
        assert!(sanitize_owner(&"x".repeat(129)).is_none());
    }
}
