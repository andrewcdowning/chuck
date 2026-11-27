# Spec: multiple users

Status: draft for review. Nothing here is built yet.

## Goal

Several people use one Chuck server. Each person's data is private to them, with one deliberate exception: tag data is pooled
to improve tag suggestions for everyone, under the limits below.

## Where things stand

- **Identity:** pluggable `Authenticator`s already turn a request into an owner id (`local`, `token`, `header`, `jwt`, or a
  provider your own build registers).
- **Isolation:** every note, tag, reminder, todo, auto-tag rule and suggestion event is stored with an owner, and every query
  filters by it. An audit of every SQL statement (November 2026) found no unscoped access: the statements without an owner
  filter are schema migrations or are keyed by a note id already checked against its owner. Another user's note id returns 404.
- **Gaps:** no user records, no way to list, disable or delete a user, no per-user export or quota, isolation is not covered by
  tests across every endpoint, and the tag model learns only from one user's notes.

## Users

    users(
      id TEXT PRIMARY KEY,            -- the owner id from the authenticator
      display_name TEXT,
      role TEXT NOT NULL DEFAULT 'user' CHECK(role IN ('user','admin')),
      status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','disabled')),
      share_tags INTEGER NOT NULL DEFAULT 1,  -- contributes to and uses shared tag data (see below)
      created_at INTEGER NOT NULL,
      last_seen_at INTEGER)

- **Provisioning:** a user row is created the first time an authenticated owner id is seen (decision U1: or only by an admin
  invite). Display name comes from the identity claim when the provider has one.
- **Disabled users** get 401 on every request; their data stays until deleted.
- **Admins** manage accounts: list users, disable, delete, set quotas, view storage used. Admins cannot read other users' notes,
  attachments, rules or suggestion history. There is no "view as user" (decision U2).
- **Self-service:** each user can export all their data (notes, todos, rules, attachments) and delete their account, which
  removes every row and blob they own.
- **Local mode** stays as today: one user, `local`, no login.

## Data boundaries

Private to each user, never visible to anyone else, including admins and the shared tag model:

- note text, titles, checklists, reminders, due dates
- attachments and their file names
- auto-tag rules (their keywords are often sensitive)
- suggestion feedback (accepted and dismissed)
- search queries

Enforcement:

- Every store method takes the owner; there is no unscoped read API.
- Blob keys are per user (see the media spec), so files are never deduplicated across users.
- Ids are not secrets but are useless across users: wrong owner returns 404, the same as a missing id.
- An isolation test suite creates two users and, for every endpoint, checks that user B cannot read, change, delete, search,
  export or get suggestions from user A's data.

## Shared tag data (the exception)

Pooling tags helps in three ways: a new user gets useful suggestions before they have tagged much; tag spellings converge
(`#k8s` and `#kubernetes`); and related tags are suggested (`#lambda` with `#aws`).

### What is shared

| Level | Data | Shared? |
| --- | --- | --- |
| 1 | Tag names and how many people use each | Yes, above the threshold |
| 2 | Which tags appear together (tag pairs) | Yes, above the threshold |
| 3 | Which words in note text predict a tag | **No.** This is note content: a rare word plus a tag can reveal what someone wrote. |

So the shared model knows "`#lambda` usually comes with `#aws`" and "`#kubernetes` is a common tag", never "someone wrote
`acquisition` next to `#project-x`".

### Protections

- **k-threshold:** a tag, or a tag pair, enters the shared data only when at least k distinct users use it (k = 5 by default,
  decision S1). Counts are per user, not per note, so one heavy user cannot push a tag over the threshold alone.
- **Batch rebuilds:** shared statistics are rebuilt on a schedule (for example nightly), not live. A user cannot add a tag and
  watch a count move to learn who else uses it.
- **No exact counts shown:** suggestions from shared data are labelled "common tag" or "often used with #aws", without numbers.
- **Private tags:** tags starting with `_` (for example `#_health`) are never shared (decision S2: or a per-tag toggle).
- **Opt-out:** a user with `share_tags = 0` neither contributes nor receives shared suggestions.
- **Excluded shapes:** tags that look like email addresses, long numbers or ticket ids are not shared (decision S3).
- **Stored separately:** `shared_tag_stats` holds only aggregates; it has no owner column and no link back to notes or users.

### How suggestions use it

The personal model stays the main source. Shared data adds:

1. **Tag-name matches for unused tags:** if the text mentions a common shared tag the user has never used, suggest it,
   labelled "common tag".
2. **Related tags:** if the note has `#lambda` and the shared pair data says `#aws` usually follows, suggest `#aws`.
3. **Cold-start prior:** for users with few tagged notes, shared tag popularity fills in the base rates. Its weight shrinks as
   the user's own data grows.
4. **Alias hints:** when two shared tags have near-identical pairings (`#k8s` and `#kubernetes`), offer to merge spellings in
   the user's own notes. Never automatic.

`chuck eval-tags` gains a `--shared` flag to measure the change. Shared signals are calibrated like everything else: if they
do not help on a user's own notes, their weight drops.

## Decisions needed

- **U1 provisioning:** create users automatically on first login (recommended), or only by admin invite.
- **U2 admin access:** admins manage accounts only, with no access to content (recommended), or allow a support "view as
  user" mode with an audit log.
- **S1 threshold:** k = 5 distinct users before a tag or tag pair is shared, or another number. Small teams may need a lower
  k, which weakens anonymity.
- **S2 private tags:** `_` prefix (recommended, visible in the text), or a per-tag privacy toggle.
- **S3 sharing default:** on by default with opt-out (recommended for small trusted teams), or off by default with opt-in
  (safer for a wider rollout at work).
- **S4 tag names themselves:** even with the threshold, a shared tag name can say something (`#layoffs`). Accept that, or add
  an admin review queue before a new tag enters the shared vocabulary.

## Phases

1. `users` table, provisioning, disable and delete, per-user export, and the isolation test suite.
2. Quotas (needed once media cards land) and the admin screen.
3. Shared tag data: aggregation job, protections, suggestion signals, `eval-tags --shared`.
