use chuck_control::{AccountRecords, OwnerRecords, Usage};
use chuck_core::domain::order::Slot;
use chuck_core::domain::schedule::Schedule;
use chuck_data::blob::BlobRef;
use chuck_data::records::{AttachmentRecord, NoteRecord, NoteRecords, RecordQuery, RecordTx};
use chuck_data::store::*;
use chuck_data::tagmodel::{FeedbackCount, HasTagModel, ModelCache, TagModelService, TagSource};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SqliteStore {
    conn: Mutex<Connection>,
    /// Per owner, bumped whenever the text of one of their cards changes (see `TagSource::training_version`).
    versions: Mutex<HashMap<String, u64>>,
    models: ModelCache,
}

/// One step of the schema's history.
pub type MigrationStep = fn(&Connection) -> Result<()>;

/// The schema's history, in order. Step N takes a database from version N-1 to version N, which is recorded in SQLite's
/// `user_version`. A released step is never edited: a schema change is a new step at the end. Each step runs in its own
/// transaction together with the version bump, so a failure leaves the database at the previous version.
pub const MIGRATIONS: &[(&str, MigrationStep)] = &[
    ("baseline: the schema as it stood when versions were introduced", migrate_baseline),
    ("card positions become sortable text, so moving a card never renumbers the others", migrate_text_positions),
    ("cards, files and rules get text ids that do not depend on the database", migrate_text_ids),
];

/// Brings the database up to date. Refuses a database from a newer program instead of guessing.
pub fn run_migrations(conn: &mut Connection, steps: &[(&str, MigrationStep)]) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let latest = steps.len() as i64;
    if current > latest {
        return Err(StoreError::Other(format!(
            "this database is at schema version {current}, but this version of chuck only knows versions up to {latest}; use a newer chuck"
        )));
    }
    for (i, (name, step)) in steps.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        step(&tx).map_err(|e| StoreError::Other(format!("schema step {} ({name}) failed: {e:?}", i + 1)))?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}

/// Step 2. Positions were numbers (REAL), with a renumbering pass when a gap ran out of precision. They become text that
/// sorts in display order and always has room between two values. Every owner's cards keep exactly the order they had:
/// the numbers are replaced by evenly spread text keys in the same order (highest first, ties by newest).
fn migrate_text_positions(conn: &Connection) -> Result<()> {
    conn.execute_batch("ALTER TABLE notes ADD COLUMN pos TEXT NOT NULL DEFAULT '';")?;
    let rows: Vec<(String, i64)> = {
        let mut st = conn.prepare("SELECT owner, id FROM notes ORDER BY owner, position DESC, id DESC")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        v
    };
    let mut i = 0;
    while i < rows.len() {
        let owner = &rows[i].0;
        let group: Vec<i64> = rows[i..].iter().take_while(|(o, _)| o == owner).map(|(_, id)| *id).collect();
        // the first card in display order gets the largest key
        let keys = chuck_core::domain::order::spread(group.len());
        for (id, key) in group.iter().zip(keys.iter().rev()) {
            conn.execute("UPDATE notes SET pos=?1 WHERE id=?2", params![key, id])?;
        }
        i += group.len();
    }
    conn.execute_batch(
        "DROP INDEX IF EXISTS notes_owner_position;
         ALTER TABLE notes DROP COLUMN position;
         ALTER TABLE notes RENAME COLUMN pos TO position;
         CREATE INDEX notes_owner_position ON notes(owner, position);",
    )?;
    Ok(())
}

/// Step 3. Ids were the database's row numbers, which a database reuses after deleting the highest one and which no other
/// store can honour. They become 26-character text ids (see `chuck_core::domain::id`). Existing rows get new ids stamped
/// with their creation time, strictly increasing, so ids sort in the order things were created. Every reference (tags, files)
/// is carried over, and the search index is rebuilt over the new tables' row numbers.
fn migrate_text_ids(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TEMP TABLE map_notes(old INTEGER PRIMARY KEY, new TEXT NOT NULL);
         CREATE TEMP TABLE map_attachments(old INTEGER PRIMARY KEY, new TEXT NOT NULL);
         CREATE TEMP TABLE map_rules(old INTEGER PRIMARY KEY, new TEXT NOT NULL);",
    )?;
    for (table, map) in [("notes", "map_notes"), ("attachments", "map_attachments"), ("tag_rules", "map_rules")] {
        let rows: Vec<(i64, i64)> = {
            let mut st = conn.prepare(&format!("SELECT id, created_at FROM {table} ORDER BY created_at, id"))?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
            v
        };
        let mut last_ms = 0u64;
        for (old, created_at) in rows {
            // the creation time, nudged forward when several rows share a second, so every id is larger than the one before
            last_ms = ((created_at.max(0) as u64) * 1000).max(last_ms + 1);
            conn.execute(&format!("INSERT INTO {map}(old, new) VALUES(?1, ?2)"), params![old, chuck_data::ids::new_id_at(last_ms)])?;
        }
    }
    conn.execute_batch(
        "CREATE TABLE notes_v3(
           id TEXT PRIMARY KEY,
           owner TEXT NOT NULL,
           body TEXT NOT NULL,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL,
           remind_at INTEGER,
           repeat TEXT,
           reminder_done INTEGER NOT NULL DEFAULT 0,
           due_at INTEGER,
           open_todos INTEGER NOT NULL DEFAULT 0,
           position TEXT NOT NULL DEFAULT '');
         INSERT INTO notes_v3(id, owner, body, created_at, updated_at, remind_at, repeat, reminder_done, due_at, open_todos, position)
           SELECT m.new, n.owner, n.body, n.created_at, n.updated_at, n.remind_at, n.repeat, n.reminder_done, n.due_at, n.open_todos, n.position
           FROM notes n JOIN map_notes m ON m.old = n.id ORDER BY n.id;
         CREATE TABLE note_tags_v3(
           note_id TEXT NOT NULL REFERENCES notes_v3(id) ON DELETE CASCADE,
           tag TEXT NOT NULL,
           PRIMARY KEY(note_id, tag));
         INSERT INTO note_tags_v3(note_id, tag) SELECT m.new, t.tag FROM note_tags t JOIN map_notes m ON m.old = t.note_id;
         CREATE TABLE attachments_v3(
           id TEXT PRIMARY KEY,
           owner TEXT NOT NULL,
           note_id TEXT NOT NULL REFERENCES notes_v3(id) ON DELETE CASCADE,
           position INTEGER NOT NULL,
           blob_key TEXT NOT NULL,
           thumb_key TEXT,
           sha256 TEXT NOT NULL,
           mime TEXT NOT NULL,
           bytes INTEGER NOT NULL,
           width INTEGER,
           height INTEGER,
           name TEXT,
           created_at INTEGER NOT NULL,
           duration_ms INTEGER);
         INSERT INTO attachments_v3(id, owner, note_id, position, blob_key, thumb_key, sha256, mime, bytes, width, height, name, created_at, duration_ms)
           SELECT ma.new, a.owner, mn.new, a.position, a.blob_key, a.thumb_key, a.sha256, a.mime, a.bytes, a.width, a.height, a.name, a.created_at, a.duration_ms
           FROM attachments a JOIN map_attachments ma ON ma.old = a.id JOIN map_notes mn ON mn.old = a.note_id;
         CREATE TABLE tag_rules_v3(
           id TEXT PRIMARY KEY,
           owner TEXT NOT NULL,
           pattern TEXT NOT NULL,
           tag TEXT NOT NULL,
           created_at INTEGER NOT NULL);
         INSERT INTO tag_rules_v3(id, owner, pattern, tag, created_at)
           SELECT mr.new, r.owner, r.pattern, r.tag, r.created_at FROM tag_rules r JOIN map_rules mr ON mr.old = r.id;

         DROP TABLE notes_fts;
         DROP TABLE note_tags;
         DROP TABLE attachments;
         DROP TABLE tag_rules;
         DROP TABLE notes;
         ALTER TABLE notes_v3 RENAME TO notes;
         ALTER TABLE note_tags_v3 RENAME TO note_tags;
         ALTER TABLE attachments_v3 RENAME TO attachments;
         ALTER TABLE tag_rules_v3 RENAME TO tag_rules;

         CREATE VIRTUAL TABLE notes_fts USING fts5(body);
         INSERT INTO notes_fts(rowid, body) SELECT rowid, body FROM notes;
         CREATE INDEX notes_owner_created ON notes(owner, created_at DESC);
         CREATE INDEX notes_owner_position ON notes(owner, position);
         CREATE INDEX notes_remind ON notes(owner, remind_at);
         CREATE INDEX note_tags_tag ON note_tags(tag);
         CREATE INDEX attachments_note ON attachments(note_id, position);
         CREATE INDEX attachments_owner_blob ON attachments(owner, blob_key);
         CREATE INDEX tag_rules_owner ON tag_rules(owner);
         DROP TABLE map_notes;
         DROP TABLE map_attachments;
         DROP TABLE map_rules;",
    )?;
    Ok(())
}

/// Everything that existed before versions were introduced. Written to be repeatable: databases from any earlier release
/// (which carry no version) are brought up to the same shape, then stamped as version 1.
fn migrate_baseline(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notes(
               id INTEGER PRIMARY KEY,
               owner TEXT NOT NULL,
               body TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS notes_owner_created ON notes(owner, created_at DESC);
             CREATE TABLE IF NOT EXISTS note_tags(
               note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
               tag TEXT NOT NULL,
               PRIMARY KEY(note_id, tag));
             CREATE INDEX IF NOT EXISTS note_tags_tag ON note_tags(tag);
             CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(body);
             CREATE TABLE IF NOT EXISTS tag_rules(
               id INTEGER PRIMARY KEY,
               owner TEXT NOT NULL,
               pattern TEXT NOT NULL,
               tag TEXT NOT NULL,
               created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS tag_rules_owner ON tag_rules(owner);
             CREATE TABLE IF NOT EXISTS suggestion_events(
               id INTEGER PRIMARY KEY,
               owner TEXT NOT NULL,
               tag TEXT NOT NULL,
               action TEXT NOT NULL CHECK(action IN ('accepted','dismissed')),
               confidence REAL NOT NULL,
               created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS suggestion_events_owner ON suggestion_events(owner, tag);
             CREATE TABLE IF NOT EXISTS attachments(
               id INTEGER PRIMARY KEY,
               owner TEXT NOT NULL,
               note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
               position INTEGER NOT NULL,
               blob_key TEXT NOT NULL,
               thumb_key TEXT,
               sha256 TEXT NOT NULL,
               mime TEXT NOT NULL,
               bytes INTEGER NOT NULL,
               width INTEGER,
               height INTEGER,
               name TEXT,
               created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS attachments_note ON attachments(note_id, position);
             CREATE INDEX IF NOT EXISTS attachments_owner_blob ON attachments(owner, blob_key);",
        )?;
        // Additive migration for databases created before reminders existed.
        let has: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name='remind_at'", [], |r| r.get(0))?;
        if has == 0 {
            conn.execute_batch(
                "ALTER TABLE notes ADD COLUMN remind_at INTEGER;
                 ALTER TABLE notes ADD COLUMN repeat TEXT;
                 ALTER TABLE notes ADD COLUMN reminder_done INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS notes_remind ON notes(owner, remind_at);",
            )?;
        }
        // Todo cards: a due date, and a maintained count of open items so they can be pinned above other cards.
        let has: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name='open_todos'", [], |r| r.get(0))?;
        if has == 0 {
            conn.execute_batch(
                "ALTER TABLE notes ADD COLUMN due_at INTEGER;
                 ALTER TABLE notes ADD COLUMN open_todos INTEGER NOT NULL DEFAULT 0;",
            )?;
            let rows: Vec<(i64, String)> = {
                let mut st = conn.prepare("SELECT id, body FROM notes WHERE body LIKE '%[ ]%'")?;
                let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
                v
            };
            for (id, body) in rows {
                conn.execute("UPDATE notes SET open_todos=?1 WHERE id=?2", params![open_items(&body), id])?;
            }
        }
        // Audio length on attachments.
        let has: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('attachments') WHERE name='duration_ms'", [], |r| r.get(0))?;
        if has == 0 {
            conn.execute_batch("ALTER TABLE attachments ADD COLUMN duration_ms INTEGER;")?;
        }
        // Accounts. Existing databases get a row for every owner that already has data.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users(
               id TEXT PRIMARY KEY,
               display_name TEXT,
               role TEXT NOT NULL DEFAULT 'user' CHECK(role IN ('user','admin')),
               status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','disabled','deleting')),
               created_at INTEGER NOT NULL,
               last_seen_at INTEGER);",
        )?;
        // Older databases allowed only 'active' and 'disabled'. Rebuild the table (a few rows) to also allow 'deleting'.
        let users_sql: String = conn.query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='users'", [], |r| r.get(0))?;
        if !users_sql.contains("'deleting'") {
            conn.execute_batch(
                "CREATE TABLE users_new(
                   id TEXT PRIMARY KEY,
                   display_name TEXT,
                   role TEXT NOT NULL DEFAULT 'user' CHECK(role IN ('user','admin')),
                   status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','disabled','deleting')),
                   created_at INTEGER NOT NULL,
                   last_seen_at INTEGER);
                 INSERT INTO users_new SELECT id, display_name, role, status, created_at, last_seen_at FROM users;
                 DROP TABLE users;
                 ALTER TABLE users_new RENAME TO users;",
            )?;
        }
        conn.execute(
            "INSERT OR IGNORE INTO users(id, created_at)
             SELECT owner, ?1 FROM (SELECT owner FROM notes UNION SELECT owner FROM tag_rules UNION SELECT owner FROM suggestion_events)",
            [now()],
        )?;
        // Manual order: a position per card, highest first within each group. Backfilled from the order cards had
        // before, so nobody's board changes when this arrives.
        let has: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name='position'", [], |r| r.get(0))?;
        if has == 0 {
            conn.execute_batch("ALTER TABLE notes ADD COLUMN position REAL NOT NULL DEFAULT 0;")?;
            let ids: Vec<(String, i64)> = {
                let mut st = conn.prepare(
                    "SELECT owner, id FROM notes ORDER BY owner, (open_todos > 0) DESC,
                       CASE WHEN open_todos > 0 THEN COALESCE(due_at, 9223372036854775807) END ASC, created_at DESC, id DESC",
                )?;
                let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
                v
            };
            let mut i = 0usize;
            while i < ids.len() {
                let owner = &ids[i].0;
                let group: Vec<i64> = ids[i..].iter().take_while(|(o, _)| o == owner).map(|(_, id)| *id).collect();
                for (k, id) in group.iter().enumerate() {
                    conn.execute("UPDATE notes SET position=?1 WHERE id=?2", params![(group.len() - k) as f64, id])?;
                }
                i += group.len();
            }
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS notes_owner_position ON notes(owner, position);")?;
    Ok(())
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl SqliteStore {
    pub fn open(path: &str) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        run_migrations(&mut conn, MIGRATIONS)?;
        Ok(Self { conn: Mutex::new(conn), versions: Mutex::new(HashMap::new()), models: ModelCache::default() })
    }

    fn rule_pairs(conn: &Connection, owner: &str) -> Result<Vec<(String, String)>> {
        let mut st = conn.prepare("SELECT pattern, tag FROM tag_rules WHERE owner=?1 ORDER BY created_at, id")?;
        let rows = st.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

}

const ATTACHMENT_COLS: &str = "id, note_id, mime, bytes, width, height, name, blob_key, thumb_key, duration_ms, sha256";

fn attachment_row(r: &rusqlite::Row) -> rusqlite::Result<AttachmentRecord> {
    let blob_key: String = r.get(7)?;
    let thumb_key: Option<String> = r.get(8)?;
    Ok(AttachmentRecord {
        id: r.get(0)?,
        note_id: r.get(1)?,
        mime: r.get(2)?,
        bytes: r.get(3)?,
        width: r.get(4)?,
        height: r.get(5)?,
        duration_ms: r.get(9)?,
        name: r.get(6)?,
        sha256: r.get(10)?,
        file: BlobRef::from_stored(blob_key),
        preview: thumb_key.map(BlobRef::from_stored),
    })
}

/// The FTS5 expression for search terms: each term quoted and prefix-matched.
fn fts_match(terms: &[String]) -> String {
    terms.iter().map(|w| format!("\"{w}\"*")).collect::<Vec<_>>().join(" ")
}

/// One SQL transaction. Writes that change card text record the owner in `changed`, which moves that owner's
/// `training_version` on commit.
struct SqliteTx<'c> {
    tx: rusqlite::Transaction<'c>,
    changed: HashSet<String>,
}

impl SqliteTx<'_> {
    fn owns(&self, owner: &str, id: &str) -> Result<bool> {
        let n: i64 = self.tx.query_row("SELECT COUNT(*) FROM notes WHERE id=?1 AND owner=?2", params![id, owner], |r| r.get(0))?;
        Ok(n > 0)
    }

    fn replace_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.tx.execute("DELETE FROM note_tags WHERE note_id=?1", [id])?;
        for t in tags {
            self.tx.execute("INSERT OR IGNORE INTO note_tags(note_id, tag) VALUES(?1, ?2)", params![id, t])?;
        }
        Ok(())
    }

    /// Cards with their tags and attachments, in the order of `ids`, fetched a page at a time instead of per card.
    fn load_notes(&self, owner: &str, ids: &[String]) -> Result<Vec<NoteRecord>> {
        let mut by_id: HashMap<String, NoteRecord> = HashMap::new();
        for chunk in ids.chunks(400) {
            let ph = vec!["?"; chunk.len()].join(",");
            let ids_only: Vec<rusqlite::types::Value> = chunk.iter().map(|i| i.clone().into()).collect();
            let mut with_owner: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
            with_owner.extend(ids_only.iter().cloned());

            let sql = format!(
                "SELECT id, body, created_at, updated_at, remind_at, repeat, reminder_done, due_at, open_todos, position FROM notes WHERE owner=? AND id IN ({ph})"
            );
            let mut st = self.tx.prepare(&sql)?;
            let rows = st.query_map(params_from_iter(with_owner.iter()), |r| {
                Ok(NoteRecord {
                    id: r.get(0)?,
                    body: r.get(1)?,
                    created_at: r.get(2)?,
                    updated_at: r.get(3)?,
                    schedule: Schedule { remind_at: r.get(4)?, repeat: r.get(5)?, reminder_done: r.get::<_, i64>(6)? != 0, due_at: r.get(7)? },
                    open_todos: r.get(8)?,
                    position: r.get(9)?,
                    tags: vec![],
                    attachments: vec![],
                })
            })?;
            for row in rows {
                let n = row?;
                by_id.insert(n.id.clone(), n);
            }

            let sql = format!("SELECT note_id, tag FROM note_tags WHERE note_id IN ({ph}) ORDER BY tag");
            let mut st = self.tx.prepare(&sql)?;
            let rows = st.query_map(params_from_iter(ids_only.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, tag) = row?;
                if let Some(n) = by_id.get_mut(&id) {
                    n.tags.push(tag);
                }
            }

            let sql = format!("SELECT {ATTACHMENT_COLS} FROM attachments WHERE owner=? AND note_id IN ({ph}) ORDER BY position, id");
            let mut st = self.tx.prepare(&sql)?;
            let rows = st.query_map(params_from_iter(with_owner.iter()), attachment_row)?;
            for row in rows {
                let a = row?;
                if let Some(n) = by_id.get_mut(&a.note_id) {
                    n.attachments.push(a);
                }
            }
        }
        Ok(ids.iter().filter_map(|i| by_id.remove(i)).collect())
    }
}

impl RecordTx for SqliteTx<'_> {
    fn insert_note(&mut self, owner: &str, n: &NoteRecord) -> Result<()> {
        let s = &n.schedule;
        self.tx.execute(
            "INSERT INTO notes(id, owner, body, created_at, updated_at, remind_at, repeat, reminder_done, due_at, open_todos, position)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![n.id, owner, n.body, n.created_at, n.updated_at, s.remind_at, s.repeat, s.reminder_done as i64, s.due_at, n.open_todos, n.position],
        )?;
        // the search index is keyed by the table's own row number, which is not the id
        let rowid = self.tx.last_insert_rowid();
        self.tx.execute("INSERT INTO notes_fts(rowid, body) VALUES(?1, ?2)", params![rowid, n.body])?;
        self.replace_tags(&n.id, &n.tags)?;
        self.changed.insert(owner.to_string());
        Ok(())
    }

    fn get_note(&mut self, owner: &str, id: &str) -> Result<Option<NoteRecord>> {
        Ok(self.load_notes(owner, &[id.to_string()])?.pop())
    }

    fn update_note(&mut self, owner: &str, n: &NoteRecord) -> Result<()> {
        if !self.owns(owner, &n.id)? {
            return Err(StoreError::NotFound);
        }
        let s = &n.schedule;
        self.tx.execute(
            "UPDATE notes SET body=?1, updated_at=?2, remind_at=?3, repeat=?4, reminder_done=?5, due_at=?6, open_todos=?7 WHERE id=?8 AND owner=?9",
            params![n.body, n.updated_at, s.remind_at, s.repeat, s.reminder_done as i64, s.due_at, n.open_todos, n.id, owner],
        )?;
        self.tx.execute("UPDATE notes_fts SET body=?1 WHERE rowid=(SELECT rowid FROM notes WHERE id=?2)", params![n.body, n.id])?;
        self.replace_tags(&n.id, &n.tags)?;
        self.changed.insert(owner.to_string());
        Ok(())
    }

    fn set_tags(&mut self, owner: &str, id: &str, tags: &[String]) -> Result<()> {
        if !self.owns(owner, id)? {
            return Err(StoreError::NotFound);
        }
        self.replace_tags(id, tags)
    }

    fn touch_note(&mut self, owner: &str, id: &str, updated_at: i64) -> Result<()> {
        self.tx.execute("UPDATE notes SET updated_at=?1 WHERE id=?2 AND owner=?3", params![updated_at, id, owner])?;
        Ok(())
    }

    fn delete_note(&mut self, owner: &str, id: &str) -> Result<bool> {
        let rowid: Option<i64> = self.tx.query_row("SELECT rowid FROM notes WHERE id=?1 AND owner=?2", params![id, owner], |r| r.get(0)).optional()?;
        let Some(rowid) = rowid else { return Ok(false) };
        self.tx.execute("DELETE FROM notes WHERE id=?1 AND owner=?2", params![id, owner])?;
        self.tx.execute("DELETE FROM notes_fts WHERE rowid=?1", [rowid])?; // tags and attachments follow by foreign key
        self.changed.insert(owner.to_string());
        Ok(true)
    }

    fn query_notes(&mut self, owner: &str, q: &RecordQuery) -> Result<Vec<NoteRecord>> {
        let mut sql = String::from("SELECT n.id FROM notes n WHERE n.owner=?");
        let mut args: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
        if !q.terms.is_empty() {
            sql.push_str(" AND n.rowid IN (SELECT rowid FROM notes_fts WHERE notes_fts MATCH ?)");
            args.push(fts_match(&q.terms).into());
        }
        if let Some(tag) = &q.tag {
            sql.push_str(" AND n.id IN (SELECT note_id FROM note_tags WHERE tag=?)");
            args.push(tag.clone().into());
        }
        if let Some(from) = q.from {
            sql.push_str(" AND n.created_at>=?");
            args.push(from.into());
        }
        if let Some(to) = q.to {
            sql.push_str(" AND n.created_at<=?");
            args.push(to.into());
        }
        if q.reminders_only {
            sql.push_str(" AND n.remind_at IS NOT NULL AND n.reminder_done=0 ORDER BY n.remind_at ASC, n.id ASC LIMIT ? OFFSET ?");
        } else {
            // Todo cards with open items stay on top until they are complete; then by manual position.
            sql.push_str(" ORDER BY (n.open_todos > 0) DESC, n.position DESC, n.id DESC LIMIT ? OFFSET ?");
        }
        args.push(q.limit.map_or(-1, |l| l as i64).into());
        args.push((q.offset as i64).into());
        let ids: Vec<String> = self.tx.prepare(&sql)?.query_map(params_from_iter(args), |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        self.load_notes(owner, &ids)
    }

    fn slot(&mut self, owner: &str, id: &str) -> Result<Option<Slot>> {
        Ok(self
            .tx
            .query_row("SELECT position, open_todos > 0, due_at FROM notes WHERE id=?1 AND owner=?2", params![id, owner], |r| {
                Ok(Slot { pos: r.get(0)?, pinned: r.get(1)?, due: r.get(2)? })
            })
            .optional()?)
    }

    fn slots(&mut self, owner: &str) -> Result<Vec<(String, Slot)>> {
        let mut st = self.tx.prepare(
            "SELECT n.id, n.position, n.open_todos > 0, n.due_at FROM notes n WHERE n.owner=?1 ORDER BY (n.open_todos > 0) DESC, n.position DESC, n.id DESC",
        )?;
        let v = st.query_map([owner], |r| Ok((r.get(0)?, Slot { pos: r.get(1)?, pinned: r.get(2)?, due: r.get(3)? })))?.collect::<std::result::Result<_, _>>()?;
        Ok(v)
    }

    fn set_position(&mut self, owner: &str, id: &str, position: &str) -> Result<()> {
        self.tx.execute("UPDATE notes SET position=?1 WHERE id=?2 AND owner=?3", params![position, id, owner])?;
        Ok(())
    }

    fn tag_counts(&mut self, owner: &str) -> Result<Vec<TagCount>> {
        let mut st = self.tx.prepare(
            "SELECT t.tag, COUNT(*) c FROM note_tags t JOIN notes n ON n.id=t.note_id
             WHERE n.owner=?1 GROUP BY t.tag ORDER BY c DESC, t.tag",
        )?;
        let rows = st.query_map([owner], |r| Ok(TagCount { tag: r.get(0)?, count: r.get(1)? }))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn rules(&mut self, owner: &str) -> Result<Vec<Rule>> {
        let mut st = self.tx.prepare("SELECT id, pattern, tag FROM tag_rules WHERE owner=?1 ORDER BY created_at, id")?;
        let rows = st.query_map([owner], |r| Ok(Rule { id: r.get(0)?, pattern: r.get(1)?, tag: r.get(2)? }))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn insert_rule(&mut self, owner: &str, id: &str, pattern: &str, tag: &str, created_at: i64) -> Result<Rule> {
        self.tx.execute("INSERT INTO tag_rules(id, owner, pattern, tag, created_at) VALUES(?1, ?2, ?3, ?4, ?5)", params![id, owner, pattern, tag, created_at])?;
        Ok(Rule { id: id.to_string(), pattern: pattern.to_string(), tag: tag.to_string() })
    }

    fn delete_rule(&mut self, owner: &str, id: &str) -> Result<bool> {
        Ok(self.tx.execute("DELETE FROM tag_rules WHERE id=?1 AND owner=?2", params![id, owner])? > 0)
    }

    fn count_attachments(&mut self, owner: &str, note_id: &str) -> Result<i64> {
        Ok(self.tx.query_row("SELECT COUNT(*) FROM attachments WHERE note_id=?1 AND owner=?2", params![note_id, owner], |r| r.get(0))?)
    }

    fn insert_attachment(&mut self, owner: &str, id: &str, note_id: &str, a: &NewAttachment, created_at: i64) -> Result<AttachmentRecord> {
        if !self.owns(owner, note_id)? {
            return Err(StoreError::NotFound);
        }
        let next: i64 = self.tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM attachments WHERE note_id=?1 AND owner=?2",
            params![note_id, owner],
            |r| r.get(0),
        )?;
        self.tx.execute(
            "INSERT INTO attachments(id, owner, note_id, position, blob_key, sha256, mime, bytes, width, height, name, created_at, duration_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![id, owner, note_id, next, a.file.key(), a.sha256, a.mime, a.bytes, a.width, a.height, a.name, created_at, a.duration_ms],
        )?;
        self.get_attachment(owner, id)?.ok_or(StoreError::NotFound)
    }

    fn get_attachment(&mut self, owner: &str, id: &str) -> Result<Option<AttachmentRecord>> {
        let sql = format!("SELECT {ATTACHMENT_COLS} FROM attachments WHERE id=?1 AND owner=?2");
        Ok(self.tx.query_row(&sql, params![id, owner], attachment_row).optional()?)
    }

    fn set_thumb(&mut self, owner: &str, id: &str, preview: &BlobRef) -> Result<()> {
        if self.get_attachment(owner, id)?.is_none() {
            return Err(StoreError::NotFound);
        }
        self.tx.execute("UPDATE attachments SET thumb_key=?1 WHERE id=?2 AND owner=?3", params![preview.key(), id, owner])?;
        Ok(())
    }

    fn delete_attachment(&mut self, owner: &str, id: &str) -> Result<Option<AttachmentRecord>> {
        let Some(a) = self.get_attachment(owner, id)? else { return Ok(None) };
        self.tx.execute("DELETE FROM attachments WHERE id=?1 AND owner=?2", params![id, owner])?;
        Ok(Some(a))
    }
}

impl chuck_data::service::HasRecords for SqliteStore {
    type R = SqliteStore;
    fn records(&self) -> &SqliteStore {
        self
    }
}

impl NoteRecords for SqliteStore {
    fn transact<T>(&self, f: impl FnOnce(&mut dyn RecordTx) -> Result<T>) -> Result<T> {
        let mut conn = chuck_core::sync::lock(&self.conn);
        let mut t = SqliteTx { tx: conn.transaction()?, changed: HashSet::new() };
        let out = f(&mut t);
        let changed = std::mem::take(&mut t.changed);
        match out {
            Ok(v) => {
                t.tx.commit()?;
                self.bump_versions(changed);
                Ok(v)
            }
            Err(e) => Err(e), // dropping the transaction rolls it back
        }
    }
}

impl SqliteStore {
    fn bump_versions(&self, owners: impl IntoIterator<Item = String>) {
        let mut v = chuck_core::sync::lock(&self.versions);
        for o in owners {
            *v.entry(o).or_insert(0) += 1;
        }
    }
}

impl TagSource for SqliteStore {
    fn training_version(&self, owner: &str) -> Result<u64> {
        Ok(chuck_core::sync::lock(&self.versions).get(owner).copied().unwrap_or(0))
    }

    fn bodies(&self, owner: &str) -> Result<Vec<String>> {
        let conn = chuck_core::sync::lock(&self.conn);
        let mut st = conn.prepare("SELECT body FROM notes WHERE owner=?1")?;
        let rows = st.query_map([owner], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn rule_pairs(&self, owner: &str) -> Result<Vec<(String, String)>> {
        let conn = chuck_core::sync::lock(&self.conn);
        Self::rule_pairs(&conn, owner)
    }

    fn feedback_counts(&self, owner: &str) -> Result<Vec<FeedbackCount>> {
        let conn = chuck_core::sync::lock(&self.conn);
        let mut st = conn.prepare("SELECT tag, SUM(action='accepted'), SUM(action='dismissed') FROM suggestion_events WHERE owner=?1 GROUP BY tag")?;
        let rows = st
            .query_map([owner], |r| {
                Ok(FeedbackCount { tag: r.get(0)?, accepted: r.get::<_, i64>(1)?.max(0) as u32, dismissed: r.get::<_, i64>(2)?.max(0) as u32 })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn record_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64, at: i64) -> Result<()> {
        let conn = chuck_core::sync::lock(&self.conn);
        conn.execute(
            "INSERT INTO suggestion_events(owner, tag, action, confidence, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![owner, tag, if accepted { "accepted" } else { "dismissed" }, confidence, at],
        )?;
        Ok(())
    }

    fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>> {
        let conn = chuck_core::sync::lock(&self.conn);
        let mut st = conn.prepare("SELECT tag, action, confidence, created_at FROM suggestion_events WHERE owner=?1 ORDER BY id")?;
        let rows = st
            .query_map([owner], |r| Ok(FeedbackEvent { tag: r.get(0)?, action: r.get(1)?, confidence: r.get(2)?, created_at: r.get(3)? }))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Owners with notes in this database, and how many each.
    fn owners(&self) -> Result<Vec<(String, i64)>> {
        let conn = chuck_core::sync::lock(&self.conn);
        let mut st = conn.prepare("SELECT owner, COUNT(*) FROM notes GROUP BY owner ORDER BY owner")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User { id: r.get(0)?, display_name: r.get(1)?, role: r.get(2)?, status: r.get(3)?, created_at: r.get(4)?, last_seen_at: r.get(5)? })
}

impl AccountRecords for SqliteStore {
    fn account(&self, id: &str) -> Result<Option<User>> {
        let conn = chuck_core::sync::lock(&self.conn);
        Ok(conn.query_row("SELECT id, display_name, role, status, created_at, last_seen_at FROM users WHERE id=?1", [id], user_row).optional()?)
    }

    fn insert_account_if_absent(&self, u: &User) -> Result<bool> {
        let conn = chuck_core::sync::lock(&self.conn);
        let n = conn.execute(
            "INSERT OR IGNORE INTO users(id, display_name, role, status, created_at, last_seen_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![u.id, u.display_name, u.role, u.status, u.created_at, u.last_seen_at],
        )?;
        Ok(n > 0)
    }

    fn update_account_profile(&self, id: &str, display_name: Option<&str>, role: &str, last_seen_at: Option<i64>) -> Result<()> {
        let conn = chuck_core::sync::lock(&self.conn);
        conn.execute("UPDATE users SET display_name=?1, role=?2, last_seen_at=?3 WHERE id=?4", params![display_name, role, last_seen_at, id])?;
        Ok(())
    }

    fn set_account_status(&self, id: &str, status: &str) -> Result<bool> {
        let conn = chuck_core::sync::lock(&self.conn);
        Ok(conn.execute("UPDATE users SET status=?1 WHERE id=?2", params![status, id])? > 0)
    }

    fn list_accounts(&self) -> Result<Vec<User>> {
        let conn = chuck_core::sync::lock(&self.conn);
        let mut st = conn.prepare("SELECT id, display_name, role, status, created_at, last_seen_at FROM users ORDER BY created_at, id")?;
        let rows = st.query_map([], user_row)?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn remove_account(&self, id: &str) -> Result<bool> {
        let conn = chuck_core::sync::lock(&self.conn);
        Ok(conn.execute("DELETE FROM users WHERE id=?1", [id])? > 0)
    }
}

impl OwnerRecords for SqliteStore {
    fn usage(&self, owner: &str) -> Result<Usage> {
        let conn = chuck_core::sync::lock(&self.conn);
        let notes: i64 = conn.query_row("SELECT COUNT(*) FROM notes WHERE owner=?1", [owner], |r| r.get(0))?;
        let (attachments, storage_bytes): (i64, i64) =
            conn.query_row("SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM attachments WHERE owner=?1", [owner], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(Usage { notes, attachments, storage_bytes })
    }

    fn blob_keys(&self, owner: &str) -> Result<Vec<String>> {
        let conn = chuck_core::sync::lock(&self.conn);
        let mut st = conn.prepare("SELECT blob_key, thumb_key FROM attachments WHERE owner=?1")?;
        let rows: Vec<(String, Option<String>)> = st.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        let mut keys: Vec<String> = rows.into_iter().flat_map(|(a, b)| std::iter::once(a).chain(b)).collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    fn all_blob_keys(&self) -> Result<Vec<String>> {
        let conn = chuck_core::sync::lock(&self.conn);
        let mut st = conn.prepare("SELECT blob_key FROM attachments UNION SELECT thumb_key FROM attachments WHERE thumb_key IS NOT NULL")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn purge_rows(&self, owner: &str) -> Result<()> {
        {
            let conn = chuck_core::sync::lock(&self.conn);
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM notes_fts WHERE rowid IN (SELECT rowid FROM notes WHERE owner=?1)", [owner])?;
            tx.execute("DELETE FROM notes WHERE owner=?1", [owner])?; // tags and attachments follow by foreign key
            tx.execute("DELETE FROM attachments WHERE owner=?1", [owner])?;
            tx.execute("DELETE FROM tag_rules WHERE owner=?1", [owner])?;
            tx.execute("DELETE FROM suggestion_events WHERE owner=?1", [owner])?;
            tx.commit()?;
        }
        self.models.forget(owner);
        self.bump_versions([owner.to_string()]);
        Ok(())
    }
}

impl HasTagModel for SqliteStore {
    fn tag_model(&self) -> TagModelService<'_> {
        TagModelService::new(self, &self.models)
    }
}

impl NoteStore for SqliteStore {
    chuck_data::note_service_methods!();
    chuck_data::tag_service_methods!();

}

#[cfg(test)]
mod tests {
    /// The conflict classification works on the text SQLite gives. Pin it against real errors, so a change in the library's
    /// wording makes this fail instead of turning conflicts back into 500s.
    #[test]
    fn real_constraint_errors_are_conflicts_and_real_faults_are_not() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(id INTEGER PRIMARY KEY, parent INTEGER NOT NULL REFERENCES parent(id), kind TEXT CHECK(kind IN ('a','b')));
             CREATE TABLE named(name TEXT PRIMARY KEY, other TEXT UNIQUE);
             INSERT INTO parent VALUES(1);
             INSERT INTO named VALUES('x', 'y');",
        )
        .unwrap();
        let err = |sql: &str| StoreError::from(c.execute_batch(sql).unwrap_err());
        assert!(matches!(err("INSERT INTO named VALUES('x', 'z');"), StoreError::Conflict(_)), "text primary key");
        assert!(matches!(err("INSERT INTO named VALUES('w', 'y');"), StoreError::Conflict(_)), "unique column");
        assert!(matches!(err("INSERT INTO parent VALUES(1);"), StoreError::Conflict(_)), "integer primary key");
        assert!(matches!(err("INSERT INTO child(parent) VALUES(99);"), StoreError::Conflict(_)), "foreign key");
        assert!(matches!(err("INSERT INTO child(parent, kind) VALUES(1, 'zzz');"), StoreError::Other(_)), "a check violation is a bug, not a conflict");
        assert!(matches!(err("INSERT INTO child(kind) VALUES('a');"), StoreError::Other(_)), "not null likewise");
        assert!(matches!(err("SELECT * FROM missing;"), StoreError::Other(_)));
    }

    #[test]
    fn a_locked_or_failing_database_is_not_a_conflict() {
        let path = std::env::temp_dir().join(format!("chuck-locked-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let a = Connection::open(&path).unwrap();
        a.execute_batch("CREATE TABLE t(x); BEGIN IMMEDIATE;").unwrap(); // holds the write lock
        let b = Connection::open(&path).unwrap();
        b.busy_timeout(std::time::Duration::from_millis(0)).unwrap();
        let locked = StoreError::from(b.execute_batch("INSERT INTO t VALUES(1);").unwrap_err());
        assert!(matches!(locked, StoreError::Other(_)), "{locked:?}");
        let io = StoreError::from(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR), None));
        assert!(matches!(io, StoreError::Other(_)), "{io:?}");
        drop((a, b));
        let _ = std::fs::remove_file(&path);
    }

    use super::*;

    /// A database as every release before versions left it: the baseline schema, no version recorded.
    fn unversioned_database(path: &std::path::Path) -> Connection {
        let _ = std::fs::remove_file(path);
        let mut c = Connection::open(path).unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        run_migrations(&mut c, &MIGRATIONS[..1]).unwrap();
        c.execute_batch("PRAGMA user_version=0;").unwrap();
        c
    }

    #[test]
    fn migration_keeps_existing_order() {
        let path = std::env::temp_dir().join(format!("chuck-migrate-{}.db", std::process::id()));
        let c = unversioned_database(&path);
        // from before manual ordering: no position column. Creation times are scrambled (id * 10) so that only the old rule
        // (open todos first by due date, then newest first) can explain the order.
        c.execute_batch(
            "DROP INDEX notes_owner_position; ALTER TABLE notes DROP COLUMN position;
             INSERT INTO notes(id, owner, body, created_at, updated_at, open_todos, due_at) VALUES
               (1, 'me', 'old', 10, 10, 0, NULL),
               (2, 'me', 'late', 20, 20, 1, 300),
               (3, 'me', 'soon', 30, 30, 1, 100),
               (4, 'me', 'new', 40, 40, 0, NULL),
               (5, 'you', 'yours', 50, 50, 0, NULL);",
        )
        .unwrap();
        drop(c);
        let s = SqliteStore::open(path.to_str().unwrap()).unwrap();
        let bodies: Vec<String> = s.list("me", &NoteQuery::default()).unwrap().into_iter().map(|n| n.body).collect();
        assert_eq!(bodies, vec!["soon", "late", "new", "old"]);
        assert_eq!(s.list("you", &NoteQuery::default()).unwrap().len(), 1);
        drop(s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn existing_databases_get_user_rows() {
        let path = std::env::temp_dir().join(format!("chuck-users-migrate-{}.db", std::process::id()));
        let c = unversioned_database(&path);
        // from before accounts existed: data under two owners, and no users table
        c.execute_batch(
            "DROP TABLE users;
             INSERT INTO notes(id, owner, body, created_at, updated_at) VALUES (1, 'local', 'old note', 1, 1);
             INSERT INTO tag_rules(id, owner, pattern, tag, created_at) VALUES (1, 'rules-only', 'x', 'y', 1);",
        )
        .unwrap();
        drop(c);
        let s = SqliteStore::open(path.to_str().unwrap()).unwrap();
        let ids: Vec<String> = s.list_accounts().unwrap().into_iter().map(|u| u.id).collect();
        assert_eq!(ids, vec!["local".to_string(), "rules-only".to_string()]);
        drop(s);
        let _ = std::fs::remove_file(&path);
    }
}
