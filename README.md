# Chuck

Local-first capture app: notes with `#tags`, full-text search, date filters. Rust, axum, SQLite (FTS5).

    cargo run            # http://127.0.0.1:3000, data in ./chuck.db
    cargo test
    cargo run --example custom_auth   # shows plugging in your own auth

Env: `CHUCK_DB` (db path), `CHUCK_ADDR` (bind address, default loopback), `CHUCK_AUTH` (auth providers, see [docs/AUTH.md](docs/AUTH.md)).

## Cards

Everything on the feed or board is a card with a `kind`: `note`, `list` (a note with `- [ ]` items, with progress), `reminder`, and the virtual `todos` card that gathers every open item from every list. Feed and board only lay cards out. Add a card type by registering a renderer in `CARD_RENDERERS` in `static/index.html`.

- **Todos card:** open items grouped by source note; ticking one updates the source note.
- **Tag suggestions:** while you type, Chuck suggests tags you already use, from keyword co-occurrence in your own notes. Local only, no external calls.
- **Auto-tag rules:** "if a note contains `jira` or `PR-`, add `#work`". Applied on save, and to existing notes on demand. See the sidebar panel.

## Extension points

- `store::NoteStore`: storage trait. Add Postgres/DynamoDB by implementing it and choosing it in `main.rs`.
- `auth::Authenticator`: returns the owner id per request. Built in: `local`, `token`, `header` (trusted proxy header), `jwt` (OIDC/ALB/Cognito). Your own provider (company SSO, Midway, anything) plugs in from your repo without forking: see [docs/AUTH.md](docs/AUTH.md) and `examples/custom_auth.rs`. Every store call is scoped by owner.
- `api.rs`: thin JSON API (`/api/notes`, `/api/tags`, `/healthz`). Capture clients (CLI, browser extension, hotkey) only need these endpoints.

## Hosting later on AWS

See [docs/AWS.md](docs/AWS.md). A `Dockerfile` is included. Configure `CHUCK_AUTH` before exposing it; Chuck refuses to listen on a non-loopback address without authentication.

## License

MIT. See [LICENSE](LICENSE).
