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

## Accounts and administrators

Each user id gets an account on its first authenticated request. Accounts are strictly separate: nobody, including an
administrator, can read another account's notes, files or rules. Administrators are the ids listed in `CHUCK_ADMINS`
(comma separated; with `CHUCK_AUTH=local` the user `local` is an administrator). The role is re-read from the config on
every request. An administrator can see each account's counts and storage size, disable or enable it, and delete it
(never their own account). A disabled account gets 401 "this account is disabled" on every endpoint. Chuck logs a
warning at start-up if no administrator is configured.

Requests are authenticated once, at the edge, and the account is looked up once per `CHUCK_ACCOUNT_CACHE_SECS` seconds
(default 30). Disabling or deleting an account takes effect at once in the process that did it, and within that many
seconds on any other process running against the same database. Set it to 0 to read the account on every request.

Deleting an account is a two-step command. The account is first marked `deleting` and refused on every request (401 "this
account is being deleted"); then its files and records are erased; only then is the account removed. If the erase fails,
the account stays `deleting` in the administrator list, and deleting it again finishes the job. A `deleting` account
cannot be enabled.

Administrators can also run the file sweep on request (`POST /api/admin/maintenance/sweep`, counts only). The server
runs it by itself every `CHUCK_SWEEP_MINUTES` minutes (default 10; 0 turns it off).

Users can export everything they own (`GET /api/me/export`, a ZIP) and delete their own account (`DELETE /api/me` with
`{"confirm": "<own id>"}`). A deleted account is erased completely; signing in again creates a new empty account, so to
keep someone out, disable the account instead.

Custom providers can implement `Authenticator::identity` (returns `Identity { id, name }`) to supply a display name;
the default calls `authenticate` and has no name. For `jwt`, `CHUCK_JWT_NAME_CLAIM` (default `name`) names the claim.

## `header`: trust an identity header from a proxy

A header alone is forgeable by anyone who can reach Chuck. So a request is accepted only if it also carries a shared secret
header that the proxy adds. Keep the secret where users cannot read it (a CloudFront custom origin header, an nginx
`proxy_set_header`) and restrict the network so only the proxy can connect.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CHUCK_AUTH_SECRET` | required, 16+ chars | Shared secret the proxy sends. |
| `CHUCK_AUTH_SECRET_HEADER` | `x-chuck-proxy-secret` | Header carrying the secret. |
| `CHUCK_AUTH_HEADER` | `x-forwarded-user` | Header carrying the user id (login name, email, employee id). |
| `CHUCK_AUTH_NAME_HEADER` | unset | Optional header with a display name (shown in the account panel; max 80 chars). |

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

## Plugging in your own provider

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

Create `crates/chuck-http/src/auth/<name>.rs`, implement `Authenticator`, and add a branch in `from_env` in `crates/chuck-http/src/auth/mod.rs`. See `jwt.rs`
for a pattern that tests with a throwaway signing key.
