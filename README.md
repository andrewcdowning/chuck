# Chuck

Local-first capture app: notes with `#tags`, full-text search, date filters. Rust, axum, SQLite (FTS5).

    cargo run            # http://127.0.0.1:3000, data in ./chuck.db
    cargo test

Env: `CHUCK_DB` (db path), `CHUCK_ADDR` (bind address, default loopback), `CHUCK_TOKEN` (optional bearer token).

## Extension points

- `store::NoteStore`: storage trait. Add Postgres/DynamoDB by implementing it and choosing it in `main.rs`.
- `auth::Authenticator`: returns the owner id per request. Every store call is scoped by owner, so multi-user hosting needs only a new authenticator (e.g. verify ALB/Cognito OIDC JWT).
- `api.rs`: thin JSON API (`/api/notes`, `/api/tags`, `/healthz`). Capture clients (CLI, browser extension, hotkey) only need these endpoints.

## Hosting later on AWS

See [docs/AWS.md](docs/AWS.md). A `Dockerfile` is included. Chuck has no real authentication yet, so do not expose it to a network before adding one.

## License

MIT. See [LICENSE](LICENSE).
