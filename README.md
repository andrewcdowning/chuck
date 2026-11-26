# Chuck

Local-first capture app: notes with `#tags`, full-text search, date filters. Rust, axum, SQLite (FTS5).

    cargo run            # http://127.0.0.1:3000, data in ./chuck.db
    cargo test
    cargo run --example custom_auth   # shows plugging in your own auth

Env: `CHUCK_DB` (db path), `CHUCK_ADDR` (bind address, default loopback), `CHUCK_AUTH` (auth providers, see [docs/AUTH.md](docs/AUTH.md)).

## Cards

Everything on the feed or board is a card with a `kind`: `note`, `list` (a note with `- [ ]` items, with progress), `reminder`, and the standalone `todos` card. Feed and board only lay cards out. Add a card type by registering a renderer in `CARD_RENDERERS` in `static/index.html`.

- **Todos card:** one standalone todo list per user. You add items to it directly; they do not belong to any note, so nothing is shown twice. Items can be ticked, edited in place and deleted, and done items cleared in one click. Write `#tags` in a todo (auto-tag rules apply too): tagged todos show as chips, count toward the sidebar tag filters, and the card follows the active tag filter, so adding a todo while viewing `#work` tags it `#work`. It is the first card in the feed and the first column on the board, and it is included in a full export.
- **Tag suggestions:** while you type, Chuck suggests tags you already use, from keyword co-occurrence in your own notes. Local only, no external calls.
- **Auto-tag rules:** "if a note contains `jira` or `PR-`, add `#work`". Applied on save, and to existing notes on demand. See the sidebar panel.

- **Export:** the **Export .md** button downloads one Markdown file, oldest first, with tags and reminders as metadata. It follows the active tag, search and date filters. API: `GET /api/export` (same filter parameters as `/api/notes`; paging is ignored).

## Extension points

- `store::NoteStore`: storage trait. Add Postgres/DynamoDB by implementing it and choosing it in `main.rs`.
- `auth::Authenticator`: returns the owner id per request. Built in: `local`, `token`, `header` (trusted proxy header), `jwt` (OIDC/ALB/Cognito). Your own provider (company SSO, Midway, anything) plugs in from your repo without forking: see [docs/AUTH.md](docs/AUTH.md) and `examples/custom_auth.rs`. Every store call is scoped by owner.
- `api.rs`: thin JSON API (`/api/notes`, `/api/tags`, `/healthz`). Capture clients (CLI, browser extension, hotkey) only need these endpoints.

## Hosting later on AWS

See [docs/AWS.md](docs/AWS.md). A `Dockerfile` is included. Configure `CHUCK_AUTH` before exposing it; Chuck refuses to listen on a non-loopback address without authentication.

## License

MIT. See [LICENSE](LICENSE).
