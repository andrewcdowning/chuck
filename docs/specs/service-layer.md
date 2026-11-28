# Spec: service layer and the three planes

Status: approved. Decisions D1 to D6 were taken as recommended. All phases (0 to 7) are built (PRs #21 to #28, and the Smithy split in #27). The packages follow the table below, with the differences noted under it.

Fixes findings A1 to A7 and B1 to B8 in [../ARCHITECTURE-REVIEW.md](../ARCHITECTURE-REVIEW.md), and prepares the split into
packages needed for API Gateway and Lambda.

## Goal

Move the behaviour out of `sqlite.rs` into a service layer, and organise the services into three planes with one-way
dependencies, so that:

1. A new storage backend implements a small record-level trait and inherits all behaviour.
2. Each plane can be packaged, scaled, authorised and deployed separately (one Lambda per plane behind API Gateway).
3. The hot path (the data plane) does not depend on account administration.

Non-goals: new features, a UI rewrite, quotas or shared tag data (they land later, in the control plane).

## The three planes

| Plane | Owns | Callers | Hot path? |
| --- | --- | --- | --- |
| **Data plane** | Users' content and what is done with it: notes, tags, search, ordering, attachments, media serving, auto-tag rules, tag suggestions and their feedback. | Signed-in users. | Yes |
| **Control plane** | Who may use the system and with what limits: account lifecycle (provision, disable, enable, delete), roles, per-user export and self-delete orchestration; later quotas and shared-tag settings. | Users (about themselves), the management plane, and the data plane (read-only, for account status). | No |
| **Management plane** | Operating the system: administrator API and screen, usage reporting, maintenance jobs (orphan file sweep, schema migration), the `eval-tags` tool, health and metrics. | Administrators and operators. | No |

### Rules between planes

1. **Dependencies point one way:** management depends on control, control depends on data. The data plane depends on
   neither. It sees only an `Account` snapshot handed to it with each request (see "Principal" below).
2. **No plane reads another plane's storage.** Cross-plane needs go through that plane's service interface. Today
   `list_users` joins notes and attachments to produce counts. After the split the control plane asks the data plane for
   `usage(owner)` instead.
3. **Content never crosses upward.** The management plane receives counts and sizes only. This keeps the existing
   guarantee that administrators cannot read content, and makes it structural instead of a convention.
4. **Cross-plane writes are commands, not shared tables.** Deleting an account is a control-plane command that calls the
   data plane's `purge(owner)` and records the outcome.

### Where each current route goes

| Plane | Routes |
| --- | --- |
| Data | `/api/notes` (list, create), `/api/notes/:id` (update, delete), `/api/notes/:id/move`, `/api/notes/:id/attachments`, `/api/tags`, `/api/export`, `/api/suggest`, `/api/suggest/feedback`, `/api/rules` (all), `/api/media/:id` and `/thumb` |
| Control | `/api/me`, `/api/me/export`, `DELETE /api/me` |
| Management | `/api/admin/users`, `/api/admin/users/status`, `/api/admin/users/delete` |

The Smithy model becomes three services (`ChuckData`, `ChuckControl`, `ChuckAdmin`) in one namespace, each producing its
own OpenAPI document. `tests/contract.rs` then checks each service against the router that serves it.

## Principal: how the planes meet on a request

Authentication happens once, at the edge of each host (axum middleware today, a Lambda authorizer later), and produces:

```rust
pub struct Principal { pub id: String, pub name: Option<String>, pub role: Role, pub status: AccountStatus }
```

- The data plane receives a `Principal` and trusts it. It checks `status == Active` and nothing else about accounts.
- The edge builds the `Principal` from the control plane's `AccountService::resolve(identity)`, which provisions on first
  sight, syncs the role from configuration, and records activity.
- `resolve` results are cached for a short time (see decision D3). This removes `touch_user` from every request (A5) and
  the double authentication.

## Service layer

Each plane has services (behaviour) over a thin store trait (records). Backends implement only store traits.

### Data plane

```rust
// Behaviour (new, in the data crate): all rules from B1 live here.
pub struct NoteService<S: NoteRecords, B: BlobStore> { /* ... */ }
impl NoteService {
    fn create(&self, p: &Principal, input: NoteInput) -> Result<Note>;
    fn update(...); fn delete(...); fn list(...); fn get(...); fn move_note(...);
    fn tags(...); fn export(...); fn apply_rules(...);
    fn usage(&self, owner: &str) -> Result<Usage>;     // for the control plane
    fn purge(&self, owner: &str) -> Result<()>;         // for the control plane, idempotent
}
pub struct MediaService<S, B> { add, get (with ranges), set_thumb, delete }
pub struct TagModelService<S: TrainingSource> { suggest, feedback, retrain policy (A1) }

// Storage (thin): records in, records out. No domain rules.
pub trait NoteRecords {
    fn put_note(&self, owner: &str, rec: NoteRecord) -> Result<()>;
    fn get_note(&self, owner: &str, id: &NoteId) -> Result<Option<NoteRecord>>;
    fn delete_note(&self, owner: &str, id: &NoteId) -> Result<()>;
    fn query_notes(&self, owner: &str, q: &RecordQuery) -> Result<Page<NoteRecord>>;  // keyed filters + cursor
    fn tag_counts(&self, owner: &str) -> Result<Vec<TagCount>>;
    fn transact<R>(&self, owner: &str, f: impl FnOnce(&mut dyn NoteTx) -> Result<R>) -> Result<R>;  // B7
    // attachments, rules, feedback events: the same style
}
pub trait TrainingSource { fn labelled_docs(&self, owner: &str) -> Result<Vec<Doc>>; fn owners(&self) -> Result<Vec<String>>; }
```

What moves into services: update semantics, reminder re-arm, card kind, tag extraction and rule application, the
attachment cap, move validation and ordering (via a fractional-index key), and the model cache with a per-owner
generation. `transact` makes the atomicity assumption explicit: SQLite implements it with a transaction, DynamoDB with
`TransactWriteItems`.

### Control plane

```rust
pub struct AccountService<S: AccountRecords> { resolve, get, set_active, delete (orchestrates purge), export (reads data plane) }
pub trait AccountRecords { fn get(&self, id: &str) -> ..; fn put(&self, a: Account) -> ..; fn list(&self) -> ..; fn delete(&self, id: &str) -> ..; }
```

Self-delete and admin delete both call `AccountService::delete`, which: marks the account `deleting`, calls the data
plane's `purge(owner)`, removes the account. If purge fails the account stays `deleting` (still refused on every
request) and the command is safe to retry. This removes today's case where files can be orphaned by a crash between the
database delete and the file delete.

### Management plane

`AdminService` lists accounts with `Usage` from the data plane, enforces "not your own account", and exposes maintenance:
`sweep_orphans()` (replaces inline deletes, fixes A3), `migrate()` (A6), and `eval_tags()` (B5, over `TrainingSource`).

## Packages

Target layout (a Cargo workspace), reached in the last phase:

| Crate | Contents | Depends on |
| --- | --- | --- |
| `chuck-core` | Types, ids, errors, pure functions (card kind, tag extraction, rule matching, ordering keys, tag model maths) | none |
| `chuck-data` | Data-plane services and store traits | core |
| `chuck-control` | Account service and store trait | core, data (the `purge` and `usage` interface only) |
| `chuck-mgmt` | Admin service, maintenance, CLI | core, control, data |
| `chuck-store-sqlite` | Implements all store traits | core, data, control |
| `chuck-http` | Router, auth middleware, `Principal` resolution; axum host | all services |
| `chuck-lambda` (later) | Lambda handlers per plane, authorizer | services, a cloud store crate |

### As built

The workspace follows the table, with these differences:

- `chuck-mgmt` holds the admin service and the sweeper but not the command-line tool. `eval-tags` opens a concrete backend
  (SQLite), so it stays in the top-level `chuck` package, which is also the binary.
- `chuck-testkit` is new: the conformance suite and the in-memory backend. It is used by tests only.
- Files go through `chuck_data::media::MediaService`, which checks an upload, keeps its bytes in the `BlobStore`, records the
  attachment and reads it back. The HTTP crate no longer touches the file store or names a storage key; what leaves the data
  plane is an `Attachment` without keys. `BlobRef` is the opaque name of a stored file.
- `OwnerRecords` and `Usage` (what the data plane tells the control plane) are in `chuck-data`, which keeps `chuck-data`
  free of any dependency on `chuck-control`. `chuck-control` re-exports them and defines `Store`, the trait that combines
  `NoteStore`, `AccountRecords` and `OwnerRecords`; servers and the HTTP edge are built on `dyn Store`.
- `Principal` is defined in `chuck-control`, so the HTTP extractor is a wrapper, `chuck_http::api::Authed`.
- `chuck-core` also holds the byte-level helpers (`media`, `export`, `zip`) and `tagging`, which depend on nothing.
- The top-level `chuck` package re-exports the old module paths (`chuck::store`, `chuck::api`, `chuck::sqlite`, ...).
- `tests/workspace.rs` fails if a crate gains a dependency that is not in the table, or if `chuck-core`, `chuck-data`,
  `chuck-control` or `chuck-mgmt` start using a web or database library.

## Phases

Each phase is one PR, behaviour-preserving unless stated, with the full test suite green. Phase 0 comes first because it
is the safety net for the rest.

0. **Conformance tests.** Move the store-level tests into a suite that runs against any `NoteStore` (ordering, search
   prefixes, update semantics, attachment cap, ownership, cascade delete). State the search contract (B3).
1. **Pure core** (built as `src/domain/`, a module for now and a crate in phase 7; the fractional-index key from D2 is not part of it and ships as its own PR with the data migration). Extract the pure domain functions into `core` modules with no store access, including ordering as a
   fractional-index key and the update/re-arm rules as functions. `sqlite.rs` calls them. Unit-tested without a database.
2. **Data services.** Introduce `NoteService`, `MediaService` and the thin `NoteRecords`/`BlobStore` traits. Rewrite
   `SqliteStore` as a records backend. Fixes A2 (batched loads), A7 (typed errors), B1, B2, B7.
3. **Tag model service.** Move suggestion logic and cache out of the store; per-owner generation (A1); `TrainingSource`;
   the CLI uses the trait (B5).
4. **Control plane.** Introduce `Principal`, `AccountService` and `AccountRecords`; authenticate once and cache account
   status (A5); account delete as a purge command with a `deleting` state; `usage()` replaces cross-table counts.
5. **Management plane.** `AdminService` over control and data interfaces; orphan sweeper replaces inline file cleanup
   (A3); numbered migrations (A6).
6. **Smithy split.** Three services and three OpenAPI documents; contract test per service. Routes keep their paths, so
   nothing changes for clients.
7. **Cargo workspace.** Move code into the crates above. No behaviour change. Lambda hosts come after this and are
   outside this spec.

Optional and breaking, decided separately: opaque string ids (B4, decision D1).

## Decisions

All six were taken as recommended (approved by the project owner).

- **D1 ids.** Done: ids of cards, files and rules are 26-character text ids (Crockford base32, ULID-shaped: creation time then
  random bits), made by the service, never by a database, strictly increasing within a process and sorting in creation
  order. Anything that is not an id answers 404 on every route. Existing databases are converted by schema step 3, which
  gives every row a new id stamped with its creation time and carries tags, files, rules and the search index over.
  This is a breaking change for anything that stored the old numeric ids.
- **D2 ordering key.** Done: positions are base-62 text keys (`0-9A-Za-z`, never ending in `0`), compared as text. There is
  always a key between two others, so a move writes one card and nothing is renumbered. Cards added on top or dragged to
  the top or bottom lengthen keys by about one character per 60 moves; moving cards into the very same gap over and over
  lengthens them by about one character per five. Existing databases are converted by schema step 2, which keeps every
  board exactly as it was (checked on a copy of a real database).
- **D3 account status cache.** The data plane accepts a cached `Principal` for a short TTL, so a disabled account may keep
  working for up to that long. Recommended 30 seconds, with an explicit "flush now" on disable inside one process. Over
  Lambda the TTL is the real bound; say if that is too loose for work use.
- **D4 sync or async store traits.** Keep synchronous (simple, matches SQLite) or go async before a network store.
  Recommended: keep sync until the cloud store exists, because the services take the trait as a parameter and the change
  would be mechanical.
- **D5 admin authorisation.** Same authenticator and an admin list (today), or a separate authorizer for the management
  plane (for example IAM on API Gateway). The structure allows either; recommended to decide when the Lambda host is built.
- **D6 search contract.** Prefix match on each word, case-insensitive, Unicode-aware, AND between words (what FTS5 does
  now), written down in phase 0 and enforced by the conformance suite.

## Risks

- Phase 2 touches every store method. The conformance suite from phase 0 and the isolation suite are the guard; run both
  on every PR in the series.
- Splitting `Principal` resolution from handlers changes the order of checks that `tests/isolation.rs` relies on. The
  "anonymous gets 401 before body parsing" behaviour must be kept.
- The `deleting` account state is new state in the account record; the admin screen and the 401 message need to handle it.
