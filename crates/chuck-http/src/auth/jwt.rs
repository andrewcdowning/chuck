//! Verifies a signed JWT from any OIDC-style identity provider.
//!
//! Works with AWS ALB OIDC/Cognito (`x-amzn-oidc-data`, keys fetched per `kid` as PEM), with any provider that
//! publishes a JWKS (Cognito, Okta, Entra ID, a corporate SSO), and with a bearer token in `Authorization`.
//! Only asymmetric algorithms are accepted, and at least one of issuer, audience or signer must be pinned so a
//! token minted by the same identity provider for a different application is not accepted.

use super::{env, sanitize_name, sanitize_owner, Authenticator, Identity};
use axum::http::{HeaderMap, HeaderName};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::Mutex,
    time::{Duration, Instant},
};

const KEY_TTL: Duration = Duration::from_secs(3600);
/// Minimum gap between outbound key fetches, so unauthenticated requests with made-up `kid`s cannot make us hammer the IdP.
const MIN_FETCH_GAP: Duration = Duration::from_secs(2);

pub trait KeySource: Send + Sync + 'static {
    fn key(&self, kid: &str) -> Option<DecodingKey>;
}

/// `kid` is attacker-controlled and may be interpolated into a URL, so allow only a tight charset.
fn valid_kid(kid: &str) -> bool {
    !kid.is_empty() && kid.len() <= 64 && kid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub struct JwtAuth {
    header: HeaderName,
    claim: String,
    /// Claim holding a display name (`CHUCK_JWT_NAME_CLAIM`, default `name`); optional in the token.
    name_claim: String,
    /// One per allowed algorithm: jsonwebtoken requires every algorithm in a `Validation` to share the key's family.
    validations: Vec<Validation>,
    keys: Box<dyn KeySource>,
    /// ALB puts the load balancer ARN in the JWT header as `signer`; pin it when set.
    signer: Option<String>,
}

impl JwtAuth {
    fn new(header: HeaderName, claim: String, name_claim: String, validations: Vec<Validation>, keys: Box<dyn KeySource>, signer: Option<String>) -> Self {
        Self { header, claim, name_claim, validations, keys, signer }
    }

    pub fn from_env() -> Result<Self, String> {
        let header: HeaderName = env("CHUCK_JWT_HEADER")
            .unwrap_or_else(|| "authorization".into())
            .parse()
            .map_err(|e| format!("CHUCK_JWT_HEADER: {e}"))?;
        let claim = env("CHUCK_JWT_CLAIM").unwrap_or_else(|| "sub".into());
        let issuer = env("CHUCK_JWT_ISSUER");
        let audience = env("CHUCK_JWT_AUDIENCE");
        let signer = env("CHUCK_JWT_SIGNER");
        if issuer.is_none() && audience.is_none() && signer.is_none() {
            return Err("CHUCK_AUTH=jwt needs at least one of CHUCK_JWT_ISSUER, CHUCK_JWT_AUDIENCE, CHUCK_JWT_SIGNER".into());
        }

        let mut algs = Vec::new();
        for a in env("CHUCK_JWT_ALGS").unwrap_or_else(|| "ES256,RS256".into()).split(',') {
            let alg = Algorithm::from_str(a.trim()).map_err(|_| format!("CHUCK_JWT_ALGS: unknown algorithm `{a}`"))?;
            if matches!(alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512) {
                return Err("CHUCK_JWT_ALGS: symmetric algorithms are not allowed".into());
            }
            algs.push(alg);
        }
        let validations = algs
            .iter()
            .map(|alg| {
                let mut v = Validation::new(*alg);
                v.leeway = 30;
                v.set_required_spec_claims(&["exp"]);
                if let Some(i) = &issuer {
                    v.set_issuer(&[i]);
                }
                match &audience {
                    Some(a) => v.set_audience(&[a]),
                    None => v.validate_aud = false,
                }
                v
            })
            .collect();

        let keys: Box<dyn KeySource> = match (env("CHUCK_JWT_JWKS_URL"), env("CHUCK_JWT_KEY_URL")) {
            (Some(u), None) => Box::new(RemoteKeys::new(Mode::Jwks(https(&u, "CHUCK_JWT_JWKS_URL")?))),
            (None, Some(u)) => {
                if !u.contains("{kid}") {
                    return Err("CHUCK_JWT_KEY_URL must contain {kid}".into());
                }
                Box::new(RemoteKeys::new(Mode::PemPerKid(https(&u, "CHUCK_JWT_KEY_URL")?)))
            }
            _ => return Err("CHUCK_AUTH=jwt needs exactly one of CHUCK_JWT_JWKS_URL or CHUCK_JWT_KEY_URL".into()),
        };
        let name_claim = env("CHUCK_JWT_NAME_CLAIM").unwrap_or_else(|| "name".into());
        Ok(Self::new(header, claim, name_claim, validations, keys, signer))
    }
}

fn https(url: &str, var: &str) -> Result<String, String> {
    if url.starts_with("https://") { Ok(url.to_string()) } else { Err(format!("{var} must be an https:// URL")) }
}

impl Authenticator for JwtAuth {
    fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        self.identity(headers).map(|i| i.id)
    }

    fn identity(&self, headers: &HeaderMap) -> Option<Identity> {
        let raw = headers.get(&self.header)?.to_str().ok()?;
        let raw = raw.strip_prefix("Bearer ").unwrap_or(raw);
        // ALB emits base64 segments with `=` padding, which JWT libraries reject. Padding carries no data.
        let token: String = raw.trim().chars().filter(|c| *c != '=').collect();

        let jh = decode_header(&token).ok()?;
        let validation = self.validations.iter().find(|v| v.algorithms.first() == Some(&jh.alg))?;
        let kid = jh.kid.as_deref().filter(|k| valid_kid(k))?;
        if let Some(expected) = &self.signer {
            let seg = token.split('.').next()?;
            let hdr: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(seg).ok()?).ok()?;
            if hdr.get("signer")?.as_str()? != expected {
                return None;
            }
        }
        let key = self.keys.key(kid)?;
        let data = decode::<serde_json::Value>(&token, &key, validation).ok()?;
        let id = sanitize_owner(data.claims.get(&self.claim)?.as_str()?)?;
        let name = data.claims.get(&self.name_claim).and_then(|v| v.as_str()).and_then(sanitize_name);
        Some(Identity { id, name })
    }
}

enum Mode {
    /// URL with `{kid}`; the response body is a PEM public key (ALB style).
    PemPerKid(String),
    /// URL of a JWKS document.
    Jwks(String),
}

struct RemoteKeys {
    mode: Mode,
    agent: ureq::Agent,
    cache: Mutex<HashMap<String, (DecodingKey, Instant)>>,
    last_fetch: Mutex<Option<Instant>>,
}

impl RemoteKeys {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build(),
            cache: Mutex::new(HashMap::new()),
            last_fetch: Mutex::new(None),
        }
    }

    fn get(&self, url: &str) -> Option<String> {
        self.agent.get(url).call().ok()?.into_string().ok()
    }
}

impl KeySource for RemoteKeys {
    fn key(&self, kid: &str) -> Option<DecodingKey> {
        if let Some((k, at)) = chuck_core::sync::lock(&self.cache).get(kid) {
            if at.elapsed() < KEY_TTL {
                return Some(k.clone());
            }
        }
        {
            let mut last = chuck_core::sync::lock(&self.last_fetch);
            if last.map_or(false, |t| t.elapsed() < MIN_FETCH_GAP) {
                return None;
            }
            *last = Some(Instant::now());
        }
        match &self.mode {
            Mode::PemPerKid(tpl) => {
                let pem = self.get(&tpl.replace("{kid}", kid))?;
                let key = DecodingKey::from_ec_pem(pem.as_bytes()).or_else(|_| DecodingKey::from_rsa_pem(pem.as_bytes())).ok()?;
                chuck_core::sync::lock(&self.cache).insert(kid.to_string(), (key.clone(), Instant::now()));
                Some(key)
            }
            Mode::Jwks(url) => {
                let set: JwkSet = serde_json::from_str(&self.get(url)?).ok()?;
                let mut cache = chuck_core::sync::lock(&self.cache);
                for jwk in &set.keys {
                    if let (Some(id), Ok(k)) = (&jwk.common.key_id, DecodingKey::from_jwk(jwk)) {
                        cache.insert(id.clone(), (k, Instant::now()));
                    }
                }
                cache.get(kid).map(|(k, _)| k.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Throwaway keypair generated for these tests only. Never used anywhere else.
    const PRIV: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPRwAtKbHGqwInOqB\n+Ib18fcHcpN16tt1qA88nPNR3SihRANCAAQCoOCJwx3e44ZxjrV35u3NtkhdXtZg\n5MS8zsRWpZE+hMgdZs2ynkZ91PJ2gQiQLI4dpS0pZdF70bVSgL9MewS6\n-----END PRIVATE KEY-----\n";
    const PUB: &str = "-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEAqDgicMd3uOGcY61d+btzbZIXV7W\nYOTEvM7EVqWRPoTIHWbNsp5GfdTydoEIkCyOHaUtKWXRe9G1UoC/THsEug==\n-----END PUBLIC KEY-----\n";

    struct Static;
    impl KeySource for Static {
        fn key(&self, kid: &str) -> Option<DecodingKey> {
            (kid == "k1").then(|| DecodingKey::from_ec_pem(PUB.as_bytes()).unwrap())
        }
    }

    fn auth(issuer: &str, audience: Option<&str>) -> JwtAuth {
        let mut v = Validation::new(Algorithm::ES256);
        v.set_required_spec_claims(&["exp"]);
        v.set_issuer(&[issuer]);
        match audience {
            Some(a) => v.set_audience(&[a]),
            None => v.validate_aud = false,
        }
        JwtAuth::new("authorization".parse().unwrap(), "sub".into(), "name".into(), vec![v], Box::new(Static), None)
    }

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    fn token(kid: &str, claims: serde_json::Value) -> String {
        let mut h = Header::new(Algorithm::ES256);
        h.kid = Some(kid.into());
        encode(&h, &claims, &EncodingKey::from_ec_pem(PRIV.as_bytes()).unwrap()).unwrap()
    }

    fn req(t: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert("authorization", format!("Bearer {t}").parse().unwrap());
        m
    }

    #[test]
    fn accepts_valid_token() {
        let t = token("k1", json!({"sub":"alice","iss":"https://idp","exp":now()+300}));
        assert_eq!(auth("https://idp", None).authenticate(&req(&t)).as_deref(), Some("alice"));
    }

    #[test]
    fn display_name_from_claim() {
        let a = auth("https://idp", None);
        let t = token("k1", json!({"sub":"alice","name":"Alice Example","iss":"https://idp","exp":now()+300}));
        assert_eq!(a.identity(&req(&t)), Some(Identity { id: "alice".into(), name: Some("Alice Example".into()) }));
        let t = token("k1", json!({"sub":"alice","name":42,"iss":"https://idp","exp":now()+300}));
        assert_eq!(a.identity(&req(&t)).unwrap().name, None, "a non-text name is ignored");
        let t = token("k1", json!({"sub":"alice","iss":"https://idp","exp":now()+300}));
        assert_eq!(a.identity(&req(&t)).unwrap().name, None);
    }

    #[test]
    fn rejects_bad_tokens() {
        let a = auth("https://idp", Some("chuck"));
        let ok = json!({"sub":"alice","iss":"https://idp","aud":"chuck","exp":now()+300});
        assert!(a.authenticate(&req(&token("k1", ok.clone()))).is_some());
        // wrong issuer, wrong audience, expired, missing exp, unknown kid, bad kid charset
        assert!(a.authenticate(&req(&token("k1", json!({"sub":"a","iss":"https://evil","aud":"chuck","exp":now()+300})))).is_none());
        assert!(a.authenticate(&req(&token("k1", json!({"sub":"a","iss":"https://idp","aud":"other","exp":now()+300})))).is_none());
        assert!(a.authenticate(&req(&token("k1", json!({"sub":"a","iss":"https://idp","aud":"chuck","exp":now()-3600})))).is_none());
        assert!(a.authenticate(&req(&token("k1", json!({"sub":"a","iss":"https://idp","aud":"chuck"})))).is_none());
        assert!(a.authenticate(&req(&token("zzz", ok.clone()))).is_none());
        assert!(a.authenticate(&req(&token("../k1", ok.clone()))).is_none());
        // tampered payload
        let t = token("k1", ok);
        let mut parts: Vec<&str> = t.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(br#"{"sub":"admin","iss":"https://idp","aud":"chuck","exp":9999999999}"#);
        parts[1] = &forged;
        assert!(a.authenticate(&req(&parts.join("."))).is_none());
        // no header, garbage
        assert!(a.authenticate(&HeaderMap::new()).is_none());
        assert!(a.authenticate(&req("not.a.jwt")).is_none());
    }

    #[test]
    fn rejects_symmetric_and_none_algs() {
        let a = auth("https://idp", None);
        let mut h = Header::new(Algorithm::HS256);
        h.kid = Some("k1".into());
        let t = encode(&h, &json!({"sub":"a","iss":"https://idp","exp":now()+300}), &EncodingKey::from_secret(PUB.as_bytes())).unwrap();
        assert!(a.authenticate(&req(&t)).is_none());
        // alg none
        let hdr = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","kid":"k1"}"#);
        let pl = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"a","iss":"https://idp","exp":{}}}"#, now() + 300));
        assert!(a.authenticate(&req(&format!("{hdr}.{pl}."))).is_none());
    }

    #[test]
    fn kid_charset() {
        assert!(valid_kid("a1b2-c3_d4"));
        assert!(!valid_kid("../etc"));
        assert!(!valid_kid("a/b"));
        assert!(!valid_kid(""));
    }
}
