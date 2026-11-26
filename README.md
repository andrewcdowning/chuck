# Chuck

Local-first capture app: notes with `#tags`, full-text search, date filters. Rust, axum, SQLite (FTS5).

    cargo run            # http://127.0.0.1:3000, data in ./chuck.db
    cargo test

Env: `CHUCK_DB` (db path), `CHUCK_ADDR` (bind address, default loopback), `CHUCK_AUTH` (auth providers, see [docs/AUTH.md](docs/AUTH.md)).

## Extension points

- `store::NoteStore`: storage trait. Add Postgres/DynamoDB by implementing it and choosing it in `main.rs`.
- `auth::Authenticator`: returns the owner id per request. Built in: `local`, `token`, `header` (trusted proxy header), `jwt` (OIDC/ALB/Cognito). Every store call is scoped by owner. See [docs/AUTH.md](docs/AUTH.md).
- `api.rs`: thin JSON API (`/api/notes`, `/api/tags`, `/healthz`). Capture clients (CLI, browser extension, hotkey) only need these endpoints.

## Hosting later on AWS

See [docs/AWS.md](docs/AWS.md). A `Dockerfile` is included. Configure `CHUCK_AUTH` before exposing it; Chuck refuses to listen on a non-loopback address without authentication.

## License

MIT. See [LICENSE](LICENSE).
