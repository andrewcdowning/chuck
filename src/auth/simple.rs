use super::{env, sanitize_owner, Authenticator};
use axum::http::{HeaderMap, HeaderName};

/// Constant-time equality for secrets (length is not secret here).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Everyone is the single user "local". Safe only when the server listens on loopback.
pub struct LocalUser;

impl Authenticator for LocalUser {
    fn authenticate(&self, _headers: &HeaderMap) -> Option<String> {
        Some("local".to_string())
    }
}

/// `Authorization: Bearer <CHUCK_TOKEN>`. All token holders map to the one owner "local".
pub struct StaticToken(String);

impl StaticToken {
    pub fn from_env() -> Result<Self, String> {
        let t = env("CHUCK_TOKEN").ok_or("CHUCK_AUTH=token needs CHUCK_TOKEN")?;
        if t.len() < 16 {
            return Err("CHUCK_TOKEN must be at least 16 characters".into());
        }
        Ok(Self(t))
    }
}

impl Authenticator for StaticToken {
    fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        let v = headers.get("authorization")?.to_str().ok()?;
        ct_eq(v.strip_prefix("Bearer ")?, &self.0).then(|| "local".to_string())
    }
}

/// Trusts a user header set by something in front of Chuck (CloudFront + edge function, nginx + SSO, ...).
/// Anyone who can reach Chuck directly could forge that header, so a request is accepted only if it also carries
/// a shared secret header that the proxy adds. Keep the secret out of the browser's reach (e.g. a CloudFront
/// custom origin header) and restrict network access so only the proxy can connect.
pub struct TrustedHeader {
    user_header: HeaderName,
    secret_header: HeaderName,
    secret: String,
}

impl TrustedHeader {
    pub fn from_env() -> Result<Self, String> {
        let name = |key: &str, default: &str| -> Result<HeaderName, String> {
            env(key).unwrap_or_else(|| default.to_string()).parse::<HeaderName>().map_err(|e| format!("{key}: {e}"))
        };
        let secret = env("CHUCK_AUTH_SECRET").ok_or("CHUCK_AUTH=header needs CHUCK_AUTH_SECRET")?;
        if secret.len() < 16 {
            return Err("CHUCK_AUTH_SECRET must be at least 16 characters".into());
        }
        Ok(Self {
            user_header: name("CHUCK_AUTH_HEADER", "x-forwarded-user")?,
            secret_header: name("CHUCK_AUTH_SECRET_HEADER", "x-chuck-proxy-secret")?,
            secret,
        })
    }

    #[cfg(test)]
    fn new(user: &str, secret_header: &str, secret: &str) -> Self {
        Self { user_header: user.parse().unwrap(), secret_header: secret_header.parse().unwrap(), secret: secret.into() }
    }
}

impl Authenticator for TrustedHeader {
    fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        let presented = headers.get(&self.secret_header)?.to_str().ok()?;
        if !ct_eq(presented, &self.secret) {
            return None;
        }
        sanitize_owner(headers.get(&self.user_header)?.to_str().ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        m
    }

    #[test]
    fn trusted_header_requires_secret() {
        let a = TrustedHeader::new("x-user", "x-secret", "0123456789abcdef");
        assert_eq!(a.authenticate(&h(&[("x-user", "alice"), ("x-secret", "0123456789abcdef")])).as_deref(), Some("alice"));
        assert!(a.authenticate(&h(&[("x-user", "alice")])).is_none());
        assert!(a.authenticate(&h(&[("x-user", "alice"), ("x-secret", "wrong")])).is_none());
        assert!(a.authenticate(&h(&[("x-secret", "0123456789abcdef")])).is_none());
    }

    #[test]
    fn static_token() {
        let a = StaticToken("0123456789abcdef".into());
        assert!(a.authenticate(&h(&[("authorization", "Bearer 0123456789abcdef")])).is_some());
        assert!(a.authenticate(&h(&[("authorization", "Bearer nope")])).is_none());
        assert!(a.authenticate(&h(&[])).is_none());
    }
}
