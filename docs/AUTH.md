# Authentication

Chuck separates "who is this request from" (an `Authenticator`) from storage. Every note belongs to an owner id, and every
store call is scoped to it, so users cannot see each other's notes. Providers are chosen at startup with `CHUCK_AUTH`,
a comma-separated list tried in order (first success wins).

| Provider | `CHUCK_AUTH` | Use for |
| --- | --- | --- |
| Local | `local` (default) | Your own machine. One user, `local`. Only allowed on a loopback address. |
| Token | `token` | Scripts and CLIs. `Authorization: Bearer $CHUCK_TOKEN`. Browsers cannot use it. |
| Trusted header | `header` | A proxy or CDN authenticates the user and passes the identity in a header. |
| JWT | `jwt` | An identity provider issues signed JWTs (OIDC, Cognito, ALB, corporate SSO). |

Example: browser users through SSO plus a script token: `CHUCK_AUTH=jwt,token`.

**Safety rail:** if the only provider is `local` and `CHUCK_ADDR` is not loopback, Chuck refuses to start. Override with
`CHUCK_ALLOW_INSECURE=1` only when something else already blocks unauthenticated access (for example local Docker with the
port published on 127.0.0.1).

Note: the HTML shell at `/` is served without authentication. It contains no data; every `/api/*` call is authenticated.

## `header`: trust an identity header from a proxy

A header alone is forgeable by anyone who can reach Chuck. So a request is accepted only if it also carries a shared secret
header that the proxy adds. Keep the secret where users cannot read it (a CloudFront custom origin header, an nginx
`proxy_set_header`) and restrict the network so only the proxy can connect.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CHUCK_AUTH_SECRET` | required, 16+ chars | Shared secret the proxy sends. |
| `CHUCK_AUTH_SECRET_HEADER` | `x-chuck-proxy-secret` | Header carrying the secret. |
| `CHUCK_AUTH_HEADER` | `x-forwarded-user` | Header carrying the user id (login name, email, employee id). |

CloudFront pattern: CloudFront adds the secret as a custom origin header. An edge function (Lambda@Edge, or a CloudFront
Function if the identity is already in the viewer request) authenticates the viewer with your SSO and sets the user header.
Make sure the edge function overwrites any user header the viewer sent.

## `jwt`: verify signed tokens

Chuck verifies the signature, expiry, and the pins you configure. Only asymmetric algorithms are accepted (default
`ES256,RS256`). At least one of issuer, audience or signer must be set, otherwise a token the same IdP minted for a different
app would be accepted.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CHUCK_JWT_HEADER` | `authorization` | Header holding the token. A `Bearer ` prefix is stripped. |
| `CHUCK_JWT_CLAIM` | `sub` | Claim used as the owner id (`sub`, `email`, `preferred_username`...). |
| `CHUCK_JWT_ISSUER` | none | Required `iss`. |
| `CHUCK_JWT_AUDIENCE` | none | Required `aud`. |
| `CHUCK_JWT_SIGNER` | none | ALB only: the load balancer ARN, checked against the `signer` field in the token header. |
| `CHUCK_JWT_ALGS` | `ES256,RS256` | Allowed algorithms. |
| `CHUCK_JWT_JWKS_URL` | | https URL of the IdP's JWKS document. |
| `CHUCK_JWT_KEY_URL` | | https URL template with `{kid}` returning one PEM public key (ALB style). |

Set exactly one of `CHUCK_JWT_JWKS_URL` and `CHUCK_JWT_KEY_URL`. Keys are cached for an hour. Fetches are rate limited, and
the `kid` is restricted to `[A-Za-z0-9_-]{1,64}` before it is placed in a URL.

### AWS ALB with OIDC or Cognito

    CHUCK_AUTH=jwt
    CHUCK_JWT_HEADER=x-amzn-oidc-data
    CHUCK_JWT_ALGS=ES256
    CHUCK_JWT_KEY_URL=https://public-keys.auth.elb.<region>.amazonaws.com/{kid}
    CHUCK_JWT_SIGNER=arn:aws:elasticloadbalancing:<region>:<acct>:loadbalancer/app/<name>/<id>
    CHUCK_JWT_ISSUER=<issuer shown in the ALB's OIDC config>
    CHUCK_JWT_CLAIM=email

Lock the task security group to the ALB so requests cannot skip it.

### Cognito or any OIDC provider directly (bearer token or cookie-less API)

    CHUCK_AUTH=jwt
    CHUCK_JWT_JWKS_URL=https://cognito-idp.<region>.amazonaws.com/<pool-id>/.well-known/jwks.json
    CHUCK_JWT_ISSUER=https://cognito-idp.<region>.amazonaws.com/<pool-id>
    CHUCK_JWT_AUDIENCE=<app client id>

## Plugging in your own provider (Midway, a CDN signer, anything else)

You do not need to change or fork Chuck. Chuck is also a library: write a small binary in your own repo that registers
your provider and starts the server. The provider is then selected with `CHUCK_AUTH=<name>`, alone or in a list
(`CHUCK_AUTH=<name>,token`).

    # Cargo.toml of your own repo
    [dependencies]
    chuck = { git = "https://github.com/andrewcdowning/chuck" }
    axum = "0.7"
    tokio = { version = "1", features = ["rt-multi-thread", "macros"] }

    // src/main.rs
    use axum::http::HeaderMap;
    use chuck::auth::{sanitize_owner, Authenticator};
    use std::sync::Arc;

    struct MySso;
    impl Authenticator for MySso {
        fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
            // Verify/decode what your SSO layer sends. Return the user id, or None for 401.
            sanitize_owner(headers.get("x-your-header")?.to_str().ok()?)
        }
    }

    #[tokio::main]
    async fn main() {
        let opts = chuck::Options::from_env()
            .with_provider("mysso", || Ok(Arc::new(MySso) as Arc<dyn Authenticator>));
        if let Err(e) = chuck::run(opts).await {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }

Run it with `CHUCK_AUTH=mysso`. A complete runnable version is `examples/custom_auth.rs`
(`CHUCK_AUTH=corp CHUCK_CORP_USER_HEADER=x-forwarded-user cargo run --example custom_auth`).

What you get from Chuck: the owner id you return scopes every note, the startup safety rail applies (a non-loopback
listener with no real provider is refused), and your provider can be combined with the built-ins in `CHUCK_AUTH`.
`sanitize_owner` and `ct_eq` (constant-time compare, for shared secrets) are exported for provider authors.

The same seam exists for storage: `Options::with_store(Arc<dyn NoteStore>)` swaps SQLite for your own store.

Rules for a provider:

- `authenticate` may block briefly (the API calls it on a blocking thread), so fetching keys over the network is fine.
  Cache what you fetch.
- Treat every header as untrusted unless something upstream proves it came from your edge layer (a signature, or a shared
  secret header as in the `header` provider). Never trust a bare user header on a port users can reach directly.
- The builder closure runs at startup. Read your settings there and return `Err("...")` for bad configuration so it fails fast.

## Adding a built-in provider to this repo

Create `src/auth/<name>.rs`, implement `Authenticator`, and add a branch in `from_env` in `src/auth/mod.rs`. See `jwt.rs`
for a pattern that tests with a throwaway signing key.
