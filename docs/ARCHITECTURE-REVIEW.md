# Architecture review

Status: the findings below describe the code as it was when the review was written (PR #19). What has been done since,
under [specs/service-layer.md](specs/service-layer.md):

| Finding | Now |
| --- | --- |
| A1 one counter retrains every model | Fixed (PR #24): versions per owner, bounded cache |
| A2 N+1 queries in `list()` | Fixed (PR #23): tags and attachments are loaded per page. Not measured |
| A3 dangling-attachment race | Fixed (phase 5): no inline deletes; a sweeper deletes only unused files that are older than ten minutes, and storing an existing file refreshes its time. A window of microseconds remains between the sweeper's last check and its delete |
| A4 `NoteStore` too big | Partly (PR #25): accounts and admin are separate traits. Cards, rules, attachments and suggestions are still one trait |
| A5 per-request work | Fixed (PR #25): one authentication per request, account cached for 30 seconds |
| A6 ad-hoc migrations | Fixed (phase 5): numbered steps in `PRAGMA user_version` |
| A7 blanket error conversion | Open |
| A8 minor items | Open |
| B1 rules inside `sqlite.rs` | Fixed (PRs #22, #23): `domain` and `NoteService` |
| B2 ordering and paging are SQL tricks | Open: positions are still floats (decision D2) |
| B3 search contract | Fixed (PR #21): stated and tested for every backend |
| B4 ids are rowids | Open (decision D1) |
| B5 tag model welded to the store | Fixed (PR #24) |
| B6 storage keys in the domain | Open |
| B7 transaction assumptions | Fixed (PR #23): `transact` |
| B8 clock and sync trait | Partly: services take a clock; the traits are still synchronous (decision D4) |

Scope: the Rust server (`src/`), its tests, and the single-page UI (`static/index.html`), as of the merge of PR #19.
Method: reading the code. No profiler was run, so the performance items describe what the code implies, not measured
behaviour.

## What is sound

- Every query is scoped by owner. `tests/isolation.rs` tries every route as another user and as an administrator, and a
  meta-test fails if a route is added without being listed there.
- The three extension traits (`NoteStore`, `Authenticator`, `BlobStore`) are real seams. No `rusqlite` type appears in
  `NoteStore`, and handlers only use the trait.
- The API contract (`smithy/model/chuck.smithy`) is checked against the router by `tests/contract.rs`.
- Safe defaults: the server refuses to listen publicly without authentication, anonymous requests are rejected before
  bodies are parsed, and blob writes are atomic (temporary file, then rename).

## Findings

Ordered by importance. "Store" means `SqliteStore` in `src/sqlite.rs` unless stated.

### A1. One counter invalidates every user's tag model

`SqliteStore` keeps a single `generation` counter that is bumped on any note write by anyone. The per-owner model cache
in `model()` compares its entry against it. With several users, one person saving a note forces everyone's next
suggestion to retrain from all of their notes. The cache is an unbounded map with one entry per user.
Fix: a generation per owner, and a bounded cache.

### A2. N+1 queries under a global lock

`list()` selects ids, then calls `load()` for each, which runs about three queries per note (the note, its tags, its
attachments). A 200-note page is about 600 queries, all while holding the one `Mutex<Connection>`, so a large listing
blocks every other request. Fix: fetch tags and attachments for the whole page with `IN (...)` queries.

### A3. Dangling-attachment race in file cleanup

`collect_garbage` checks `blob_referenced`, then deletes the file, with nothing holding the two steps together. Files are
named by content, so an upload of identical bytes by the same owner that lands in that gap can have its file deleted just
after its row is inserted. The result is a card with a broken file. The window is narrow. Fix: do the check and the delete
in one critical section, or sweep unreferenced files on a schedule instead of deleting inline.

### A4. `NoteStore` has too many jobs

33 methods covering notes, attachments, rules, suggestion feedback, accounts and admin. Every new backend must implement
all of it, and a Lambda that only serves notes would still depend on the account and admin code.

### A5. Per-request work on the hot path

- `touch_user` takes the global lock on every request, for a read and sometimes a write.
- Authentication runs twice per request (the `require_identity` middleware and `session()`). With the `jwt` provider the
  token is verified twice.
Fix: authenticate once and pass the result on (for example a request extension), and cache account status for a short time.

### A6. Ad-hoc schema migrations

Migrations are `CREATE ... IF NOT EXISTS` plus checks on the existing table structure, with no schema version. They work
today. Ordering and rollback get harder with each new table (`users` is the latest, `share_tags` is planned). Fix: numbered
steps recorded in `PRAGMA user_version`.

### A7. Blanket error conversion

`impl<E: Error> From<E> for StoreError` turns every database error into `Other`, which the API reports as 500. A constraint
violation and a disk failure look the same. Handle specific SQLite codes to return 409 where that is the right answer.

### A8. Minor

- `static/index.html` is about 1,550 lines of HTML, CSS and JS in one file. Fine for one person, hard for a second.
- Reminders fire only while a tab is open (checked every 30 seconds in the page). Nothing notifies a user with the page
  closed. This is a product gap more than a code smell.
- The account export builds a ZIP in memory, capped at 1 GB. That will not survive Lambda (6 MB response limit).
- About 29 `lock().unwrap()` calls. A panic while holding the lock poisons the mutex and every later request then panics.
  Not confirmed whether the server catches panics per request.

## Store coupling to SQLite

The trait signatures are clean. The coupling is in behaviour: the SQLite file implements domain rules that a second
backend would have to copy.

### B1. Domain rules live inside `sqlite.rs`

- What "absent" and "<= 0" mean on update, and when a reminder re-arms (documented on `NoteInput`, implemented only in
  the store).
- Open-todo grouping and move validation (`move_note`).
- The 50-attachment cap, tag extraction on write, applying auto-tag rules, and `open_todos` kept as a stored column.

### B2. Ordering and paging are SQL tricks

Card order is a float `position` with midpoint insertion and a `renumber` fallback. Paging is `LIMIT ... OFFSET`. Both fit
SQL and fit key-value stores poorly.

### B3. Search semantics are FTS5's

`NoteQuery.q` is documented as "prefix match on each word", but the real behaviour is `fts_query` plus the FTS5 tokenizer.
A different backend would return different results, and no test states the contract.

### B4. Ids are SQLite rowids

`i64` ids appear in the trait, URLs and the Smithy model. SQLite reuses the highest id after a delete, which caused the
stale-picture bug fixed with the `v` version field. A cloud store would want UUIDs or ULIDs, which changes the API.

### B5. The tag model is welded to the store

`suggest_tags` is a store method, and the model cache and `generation` counter live inside `SqliteStore`. `cli.rs` calls
`SqliteStore::owners()` and `training_docs()`, which are inherent methods outside the trait, so `eval-tags` works only on
SQLite. This should be a service that reads training documents through the trait.

### B6. Storage keys leak into the domain

`NewAttachment` carries `blob_key` and `sha256`, and `Attachment` has hidden `blob_key` and `thumb_key` fields.
`blob_referenced` is a SQL lookup used for file cleanup. An S3 backend would prefer reference counts or lifecycle rules.

### B7. Unstated transaction assumptions

`add_attachment` (check cap, check ownership, insert), `delete_user` (cascade) and `apply_rules` need multi-statement
atomicity. The trait does not say so. DynamoDB would need transactional writes or would lose atomicity.

### B8. Smaller points

`now()` is called inside the store, so time cannot be injected. The trait is synchronous by design (documented), so a
network-backed store would block one `spawn_blocking` thread per call.
