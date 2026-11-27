# Chuck

Local-first capture app: notes with `#tags`, full-text search, date filters. Rust, axum, SQLite (FTS5).

    cargo run            # http://127.0.0.1:3000, data in ./chuck.db
    cargo test
    cargo run --example custom_auth   # shows plugging in your own auth

Env: `CHUCK_DB` (db path), `CHUCK_ADDR` (bind address, default loopback), `CHUCK_AUTH` (auth providers, see [docs/AUTH.md](docs/AUTH.md)).

## Cards

Everything is a card with a `kind`: `note`, `todo` (a card with `- [ ]` items, shown with progress) or `reminder`.

- **Types:** pick Note, Todo or Reminder, or type `#todo`, `#reminder` or `#note` followed by a space while writing. The composer switches type, removes the token and keeps your text. (These three words are reserved for this and are not used as tags.)
- **Todos:** open todo cards stay at the top, soonest due date first, until every item is ticked. A todo card can have a due date; overdue and due-today cards are highlighted.
- **Feed and Board:** the Feed is one column; the Board is a grid. Both show cards in creation order (open todos first) with each card's tags shown on it. There are no tag columns: pick a tag in the sidebar to see just those cards.
- **Markdown:** card text renders headings (`#`), **bold**, *italic*, ~~strikethrough~~, `code` and fenced code blocks, lists, quotes, rules and links. Rendering builds DOM nodes with `textContent` only, so raw HTML in a note is shown as text, links must be http/https/mailto, and images are not loaded. Edit a card to see the raw text.
- **Pictures:** drop images on the composer (or paste, or **Attach**, or type `#photo`) to make a picture card, or drop them on any card to add to it. Photos are resized and rotated upright in the browser; location and other metadata are stripped on the server too. One picture shows at its own proportions; several form a grid; click to view full size. Files live in a `<database name>-media/` folder beside the database (`CHUCK_MEDIA_DIR` to move it), stored once per user, and are deleted with their last card. Spec: `docs/specs/media-cards.md` (PR 10).
- **Tag suggestions:** while you type, Chuck suggests tags you already use, each with a calibrated confidence ("about this often right") learned from your own tagged notes. Accept or dismiss a suggestion to tune it. `chuck eval-tags` measures accuracy and calibration on your notes. Local only, no external calls. See [docs/TAGGING.md](docs/TAGGING.md).
- **Auto-tag rules:** "if a note contains `jira` or `PR-`, add `#work`". Applied on save, and to existing notes on demand. See the sidebar panel.
- **Export:** the **Export .md** button downloads one Markdown file, oldest first, with tags, due dates and reminders as metadata. It follows the active tag, search and date filters. API: `GET /api/export` (same filter parameters as `/api/notes`; paging is ignored).

## Extension points

- `store::NoteStore`: storage trait. Add Postgres/DynamoDB by implementing it and choosing it in `main.rs`.
- `auth::Authenticator`: returns the owner id per request. Built in: `local`, `token`, `header` (trusted proxy header), `jwt` (OIDC/ALB/Cognito). Your own provider (a company SSO, a CDN signer, anything) plugs in from your repo without forking: see [docs/AUTH.md](docs/AUTH.md) and `examples/custom_auth.rs`. Every store call is scoped by owner.
- `api.rs`: thin JSON API (`/api/notes`, `/api/tags`, `/healthz`). Capture clients (CLI, browser extension, hotkey) only need these endpoints.

## Hosting later on AWS

See [docs/AWS.md](docs/AWS.md). A `Dockerfile` is included. Configure `CHUCK_AUTH` before exposing it; Chuck refuses to listen on a non-loopback address without authentication.

## License

MIT. See [LICENSE](LICENSE).
