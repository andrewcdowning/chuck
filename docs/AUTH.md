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

## Using it with Midway, CloudFront or another corporate SSO

I could not see Amazon-internal documentation, so this maps the two shapes such systems usually take onto the providers above.
Check which one your setup uses:

1. **The SSO layer in front of Chuck forwards a signed JWT** (an OIDC token in some header): use `jwt`. You need the
   header name, the issuer, the key/JWKS URL, and the claim that holds the login name.
2. **The SSO layer authenticates and passes only a plain identity header** (the usual pattern for a CloudFront + edge
   function or reverse-proxy setup): use `header`, and have that layer also add the shared secret header.

Either way, only configuration changes; Chuck's storage and API stay the same. If neither fits, add a provider (below).

## Adding a provider

1. Create `src/auth/<name>.rs` with a type implementing `Authenticator`: read the headers, return `Some(owner)` or `None`.
   Run owner ids through `sanitize_owner`. It may block briefly, because the API calls it on a blocking thread.
2. Add a branch in `from_env` in `src/auth/mod.rs` for the new `CHUCK_AUTH` name and read its settings from environment
   variables, returning a clear `Err(String)` for bad configuration (startup fails fast).
3. Add unit tests next to it. See `jwt.rs` for a pattern that signs tokens with a throwaway test key.
