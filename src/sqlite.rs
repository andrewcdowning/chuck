use crate::domain::order::Slot;
use crate::domain::schedule::Schedule;
use crate::records::{NoteRecord, NoteRecords, RecordQuery, RecordTx};
use crate::store::*;
use crate::tagmodel::{FeedbackCount, HasTagModel, ModelCache, TagModelService, TagSource};
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

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl SqliteStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS notes(
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
               status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','disabled')),
               created_at INTEGER NOT NULL,
               last_seen_at INTEGER);",
        )?;
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
        Ok(Self { conn: Mutex::new(conn), versions: Mutex::new(HashMap::new()), models: ModelCache::default() })
    }

    fn rule_pairs(conn: &Connection, owner: &str) -> Result<Vec<(String, String)>> {
        let mut st = conn.prepare("SELECT pattern, tag FROM tag_rules WHERE owner=?1 ORDER BY id")?;
        let rows = st.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

}

const ATTACHMENT_COLS: &str = "id, note_id, mime, bytes, width, height, name, blob_key, thumb_key, duration_ms";

fn attachment_row(r: &rusqlite::Row) -> rusqlite::Result<Attachment> {
    let thumb_key: Option<String> = r.get(8)?;
    let blob_key: String = r.get(7)?;
    Ok(Attachment {
        id: r.get(0)?,
        note_id: r.get(1)?,
        mime: r.get(2)?,
        bytes: r.get(3)?,
        width: r.get(4)?,
        height: r.get(5)?,
        duration_ms: r.get(9)?,
        name: r.get(6)?,
        has_thumb: thumb_key.is_some(),
        v: crate::blob::attachment_version(&blob_key, thumb_key.as_deref()),
        blob_key,
        thumb_key,
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
    fn owns(&self, owner: &str, id: i64) -> Result<bool> {
        let n: i64 = self.tx.query_row("SELECT COUNT(*) FROM notes WHERE id=?1 AND owner=?2", params![id, owner], |r| r.get(0))?;
        Ok(n > 0)
    }

    fn replace_tags(&self, id: i64, tags: &[String]) -> Result<()> {
        self.tx.execute("DELETE FROM note_tags WHERE note_id=?1", [id])?;
        for t in tags {
            self.tx.execute("INSERT OR IGNORE INTO note_tags(note_id, tag) VALUES(?1, ?2)", params![id, t])?;
        }
        Ok(())
    }

    /// Cards with their tags and attachments, in the order of `ids`, fetched a page at a time instead of per card.
    fn load_notes(&self, owner: &str, ids: &[i64]) -> Result<Vec<NoteRecord>> {
        let mut by_id: HashMap<i64, NoteRecord> = HashMap::new();
        for chunk in ids.chunks(400) {
            let ph = vec!["?"; chunk.len()].join(",");
            let ids_only: Vec<rusqlite::types::Value> = chunk.iter().map(|i| (*i).into()).collect();
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
                by_id.insert(n.id, n);
            }

            let sql = format!("SELECT note_id, tag FROM note_tags WHERE note_id IN ({ph}) ORDER BY tag");
            let mut st = self.tx.prepare(&sql)?;
            let rows = st.query_map(params_from_iter(ids_only.iter()), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
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
    fn insert_note(&mut self, owner: &str, n: &NoteRecord) -> Result<i64> {
        let s = &n.schedule;
        self.tx.execute(
            "INSERT INTO notes(owner, body, created_at, updated_at, remind_at, repeat, reminder_done, due_at, open_todos, position)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![owner, n.body, n.created_at, n.updated_at, s.remind_at, s.repeat, s.reminder_done as i64, s.due_at, n.open_todos, n.position],
        )?;
        let id = self.tx.last_insert_rowid();
        self.tx.execute("INSERT INTO notes_fts(rowid, body) VALUES(?1, ?2)", params![id, n.body])?;
        self.replace_tags(id, &n.tags)?;
        self.changed.insert(owner.to_string());
        Ok(id)
    }

    fn get_note(&mut self, owner: &str, id: i64) -> Result<Option<NoteRecord>> {
        Ok(self.load_notes(owner, &[id])?.pop())
    }

    fn update_note(&mut self, owner: &str, n: &NoteRecord) -> Result<()> {
        if !self.owns(owner, n.id)? {
            return Err(StoreError::NotFound);
        }
        let s = &n.schedule;
        self.tx.execute(
            "UPDATE notes SET body=?1, updated_at=?2, remind_at=?3, repeat=?4, reminder_done=?5, due_at=?6, open_todos=?7 WHERE id=?8 AND owner=?9",
            params![n.body, n.updated_at, s.remind_at, s.repeat, s.reminder_done as i64, s.due_at, n.open_todos, n.id, owner],
        )?;
        self.tx.execute("UPDATE notes_fts SET body=?1 WHERE rowid=?2", params![n.body, n.id])?;
        self.replace_tags(n.id, &n.tags)?;
        self.changed.insert(owner.to_string());
        Ok(())
    }

    fn set_tags(&mut self, owner: &str, id: i64, tags: &[String]) -> Result<()> {
        if !self.owns(owner, id)? {
            return Err(StoreError::NotFound);
        }
        self.replace_tags(id, tags)
    }

    fn touch_note(&mut self, owner: &str, id: i64, updated_at: i64) -> Result<()> {
        self.tx.execute("UPDATE notes SET updated_at=?1 WHERE id=?2 AND owner=?3", params![updated_at, id, owner])?;
        Ok(())
    }

    fn delete_note(&mut self, owner: &str, id: i64) -> Result<bool> {
        let n = self.tx.execute("DELETE FROM notes WHERE id=?1 AND owner=?2", params![id, owner])?;
        if n == 0 {
            return Ok(false);
        }
        self.tx.execute("DELETE FROM notes_fts WHERE rowid=?1", [id])?; // tags and attachments follow by foreign key
        self.changed.insert(owner.to_string());
        Ok(true)
    }

    fn query_notes(&mut self, owner: &str, q: &RecordQuery) -> Result<Vec<NoteRecord>> {
        let mut sql = String::from("SELECT n.id FROM notes n WHERE n.owner=?");
        let mut args: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
        if !q.terms.is_empty() {
            sql.push_str(" AND n.id IN (SELECT rowid FROM notes_fts WHERE notes_fts MATCH ?)");
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
        let ids: Vec<i64> = self.tx.prepare(&sql)?.query_map(params_from_iter(args), |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        self.load_notes(owner, &ids)
    }

    fn slot(&mut self, owner: &str, id: i64) -> Result<Option<Slot>> {
        Ok(self
            .tx
            .query_row("SELECT position, open_todos > 0, due_at FROM notes WHERE id=?1 AND owner=?2", params![id, owner], |r| {
                Ok(Slot { pos: r.get(0)?, pinned: r.get(1)?, due: r.get(2)? })
            })
            .optional()?)
    }

    fn slots(&mut self, owner: &str) -> Result<Vec<(i64, Slot)>> {
        let mut st = self.tx.prepare(
            "SELECT n.id, n.position, n.open_todos > 0, n.due_at FROM notes n WHERE n.owner=?1 ORDER BY (n.open_todos > 0) DESC, n.position DESC, n.id DESC",
        )?;
        let v = st.query_map([owner], |r| Ok((r.get(0)?, Slot { pos: r.get(1)?, pinned: r.get(2)?, due: r.get(3)? })))?.collect::<std::result::Result<_, _>>()?;
        Ok(v)
    }

    fn set_positions(&mut self, owner: &str, positions: &[(i64, f64)]) -> Result<()> {
        for (id, pos) in positions {
            self.tx.execute("UPDATE notes SET position=?1 WHERE id=?2 AND owner=?3", params![pos, id, owner])?;
        }
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
        let mut st = self.tx.prepare("SELECT id, pattern, tag FROM tag_rules WHERE owner=?1 ORDER BY id")?;
        let rows = st.query_map([owner], |r| Ok(Rule { id: r.get(0)?, pattern: r.get(1)?, tag: r.get(2)? }))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn insert_rule(&mut self, owner: &str, pattern: &str, tag: &str, created_at: i64) -> Result<Rule> {
        self.tx.execute("INSERT INTO tag_rules(owner, pattern, tag, created_at) VALUES(?1, ?2, ?3, ?4)", params![owner, pattern, tag, created_at])?;
        Ok(Rule { id: self.tx.last_insert_rowid(), pattern: pattern.to_string(), tag: tag.to_string() })
    }

    fn delete_rule(&mut self, owner: &str, id: i64) -> Result<bool> {
        Ok(self.tx.execute("DELETE FROM tag_rules WHERE id=?1 AND owner=?2", params![id, owner])? > 0)
    }

    fn count_attachments(&mut self, owner: &str, note_id: i64) -> Result<i64> {
        Ok(self.tx.query_row("SELECT COUNT(*) FROM attachments WHERE note_id=?1 AND owner=?2", params![note_id, owner], |r| r.get(0))?)
    }

    fn insert_attachment(&mut self, owner: &str, note_id: i64, a: &NewAttachment, created_at: i64) -> Result<Attachment> {
        if !self.owns(owner, note_id)? {
            return Err(StoreError::NotFound);
        }
        let next: i64 = self.tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM attachments WHERE note_id=?1 AND owner=?2",
            params![note_id, owner],
            |r| r.get(0),
        )?;
        self.tx.execute(
            "INSERT INTO attachments(owner, note_id, position, blob_key, sha256, mime, bytes, width, height, name, created_at, duration_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![owner, note_id, next, a.blob_key, a.sha256, a.mime, a.bytes, a.width, a.height, a.name, created_at, a.duration_ms],
        )?;
        let id = self.tx.last_insert_rowid();
        self.get_attachment(owner, id)?.ok_or(StoreError::NotFound)
    }

    fn get_attachment(&mut self, owner: &str, id: i64) -> Result<Option<Attachment>> {
        let sql = format!("SELECT {ATTACHMENT_COLS} FROM attachments WHERE id=?1 AND owner=?2");
        Ok(self.tx.query_row(&sql, params![id, owner], attachment_row).optional()?)
    }

    fn set_thumb(&mut self, owner: &str, id: i64, thumb_key: &str) -> Result<Option<String>> {
        let old = self.get_attachment(owner, id)?.ok_or(StoreError::NotFound)?.thumb_key;
        self.tx.execute("UPDATE attachments SET thumb_key=?1 WHERE id=?2 AND owner=?3", params![thumb_key, id, owner])?;
        Ok(old)
    }

    fn delete_attachment(&mut self, owner: &str, id: i64) -> Result<Option<Attachment>> {
        let Some(a) = self.get_attachment(owner, id)? else { return Ok(None) };
        self.tx.execute("DELETE FROM attachments WHERE id=?1 AND owner=?2", params![id, owner])?;
        Ok(Some(a))
    }

    fn blob_referenced(&mut self, owner: &str, key: &str) -> Result<bool> {
        let n: i64 = self.tx.query_row(
            "SELECT COUNT(*) FROM attachments WHERE owner=?1 AND (blob_key=?2 OR thumb_key=?2)",
            params![owner, key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

impl crate::service::HasRecords for SqliteStore {
    type R = SqliteStore;
    fn records(&self) -> &SqliteStore {
        self
    }
}

impl NoteRecords for SqliteStore {
    fn transact<T>(&self, f: impl FnOnce(&mut dyn RecordTx) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap();
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
        let mut v = self.versions.lock().unwrap();
        for o in owners {
            *v.entry(o).or_insert(0) += 1;
        }
    }
}

impl TagSource for SqliteStore {
    fn training_version(&self, owner: &str) -> Result<u64> {
        Ok(self.versions.lock().unwrap().get(owner).copied().unwrap_or(0))
    }

    fn bodies(&self, owner: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT body FROM notes WHERE owner=?1")?;
        let rows = st.query_map([owner], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn rule_pairs(&self, owner: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        Self::rule_pairs(&conn, owner)
    }

    fn feedback_counts(&self, owner: &str) -> Result<Vec<FeedbackCount>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT tag, SUM(action='accepted'), SUM(action='dismissed') FROM suggestion_events WHERE owner=?1 GROUP BY tag")?;
        let rows = st
            .query_map([owner], |r| {
                Ok(FeedbackCount { tag: r.get(0)?, accepted: r.get::<_, i64>(1)?.max(0) as u32, dismissed: r.get::<_, i64>(2)?.max(0) as u32 })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn record_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64, at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO suggestion_events(owner, tag, action, confidence, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![owner, tag, if accepted { "accepted" } else { "dismissed" }, confidence, at],
        )?;
        Ok(())
    }

    fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT tag, action, confidence, created_at FROM suggestion_events WHERE owner=?1 ORDER BY id")?;
        let rows = st
            .query_map([owner], |r| Ok(FeedbackEvent { tag: r.get(0)?, action: r.get(1)?, confidence: r.get(2)?, created_at: r.get(3)? }))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Owners with notes in this database, and how many each.
    fn owners(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT owner, COUNT(*) FROM notes GROUP BY owner ORDER BY owner")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

impl HasTagModel for SqliteStore {
    fn tag_model(&self) -> TagModelService<'_> {
        TagModelService::new(self, &self.models)
    }
}

impl NoteStore for SqliteStore {
    crate::note_service_methods!();
    crate::tag_service_methods!();

    fn touch_user(&self, id: &str, display_name: Option<&str>, admin: bool) -> Result<User> {
        let conn = self.conn.lock().unwrap();
        let t = now();
        let role = if admin { "admin" } else { "user" };
        let cur = conn
            .query_row("SELECT display_name, role, status, created_at, last_seen_at FROM users WHERE id=?1", [id], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<i64>>(4)?))
            })
            .optional()?;
        let Some((name, cur_role, status, created_at, seen)) = cur else {
            conn.execute(
                "INSERT INTO users(id, display_name, role, status, created_at, last_seen_at) VALUES(?1, ?2, ?3, 'active', ?4, ?4)",
                params![id, display_name, role, t],
            )?;
            return Ok(User { id: id.into(), display_name: display_name.map(String::from), role: role.into(), status: "active".into(), created_at: t, last_seen_at: Some(t) });
        };
        let new_name = display_name.map(String::from).or(name.clone());
        let active = status == "active";
        let stale = seen.map_or(true, |s| t - s >= 300);
        let last_seen = if active && stale { Some(t) } else { seen };
        if cur_role != role || new_name != name || last_seen != seen {
            conn.execute("UPDATE users SET display_name=?1, role=?2, last_seen_at=?3 WHERE id=?4", params![new_name, role, last_seen, id])?;
        }
        Ok(User { id: id.into(), display_name: new_name, role: role.into(), status, created_at, last_seen_at: last_seen })
    }

    fn list_users(&self) -> Result<Vec<UserSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT u.id, u.display_name, u.role, u.status, u.created_at, u.last_seen_at,
                    (SELECT COUNT(*) FROM notes n WHERE n.owner = u.id),
                    (SELECT COUNT(*) FROM attachments a WHERE a.owner = u.id),
                    (SELECT COALESCE(SUM(a.bytes), 0) FROM attachments a WHERE a.owner = u.id)
             FROM users u ORDER BY u.created_at, u.id",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(UserSummary {
                    user: User { id: r.get(0)?, display_name: r.get(1)?, role: r.get(2)?, status: r.get(3)?, created_at: r.get(4)?, last_seen_at: r.get(5)? },
                    notes: r.get(6)?,
                    attachments: r.get(7)?,
                    storage_bytes: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn set_user_active(&self, id: &str, active: bool) -> Result<User> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("UPDATE users SET status=?1 WHERE id=?2", params![if active { "active" } else { "disabled" }, id])?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        conn.query_row("SELECT id, display_name, role, status, created_at, last_seen_at FROM users WHERE id=?1", [id], |r| {
            Ok(User { id: r.get(0)?, display_name: r.get(1)?, role: r.get(2)?, status: r.get(3)?, created_at: r.get(4)?, last_seen_at: r.get(5)? })
        })
        .map_err(Into::into)
    }

    fn delete_user(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let exists: i64 = conn.query_row("SELECT COUNT(*) FROM users WHERE id=?1", [id], |r| r.get(0))?;
        if exists == 0 {
            return Err(StoreError::NotFound);
        }
        let keys: Vec<String> = {
            let mut st = conn.prepare("SELECT blob_key, thumb_key FROM attachments WHERE owner=?1")?;
            let rows: Vec<(String, Option<String>)> = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
            let mut keys: Vec<String> = rows.into_iter().flat_map(|(a, b)| std::iter::once(a).chain(b)).collect();
            keys.sort();
            keys.dedup();
            keys
        };
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM notes_fts WHERE rowid IN (SELECT id FROM notes WHERE owner=?1)", [id])?;
        tx.execute("DELETE FROM notes WHERE owner=?1", [id])?; // tags and attachments follow by foreign key
        tx.execute("DELETE FROM attachments WHERE owner=?1", [id])?;
        tx.execute("DELETE FROM tag_rules WHERE owner=?1", [id])?;
        tx.execute("DELETE FROM suggestion_events WHERE owner=?1", [id])?;
        tx.execute("DELETE FROM users WHERE id=?1", [id])?;
        tx.commit()?;
        self.models.forget(id);
        self.bump_versions([id.to_string()]);
        Ok(keys)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_keeps_existing_order() {
        let path = std::env::temp_dir().join(format!("chuck-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = path.to_str().unwrap();
        let mk = |b: &str, due: Option<i64>| NoteInput { body: b.into(), due_at: due, ..Default::default() };
        let before: Vec<i64> = {
            let s = SqliteStore::open(p).unwrap();
            s.create("me", &mk("old", None)).unwrap();
            s.create("me", &mk("late\n- [ ] x", Some(300))).unwrap();
            s.create("me", &mk("soon\n- [ ] x", Some(100))).unwrap();
            s.create("me", &mk("new", None)).unwrap();
            s.create("you", &mk("yours", None)).unwrap();
            s.list("me", &NoteQuery::default()).unwrap().into_iter().map(|n| n.id).collect()
        };
        // simulate a database from before manual ordering
        {
            let c = Connection::open(p).unwrap();
            c.execute_batch("DROP INDEX notes_owner_position; ALTER TABLE notes DROP COLUMN position;").unwrap();
            // scramble creation times so only the old rule (todos by due date, then newest first) can explain the order
            c.execute("UPDATE notes SET created_at = id * 10", []).unwrap();
        }
        let s = SqliteStore::open(p).unwrap();
        let after: Vec<i64> = s.list("me", &NoteQuery::default()).unwrap().into_iter().map(|n| n.id).collect();
        assert_eq!(after, before);
        assert_eq!(s.list("you", &NoteQuery::default()).unwrap().len(), 1);
        drop(s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn existing_databases_get_user_rows() {
        let path = std::env::temp_dir().join(format!("chuck-users-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = path.to_str().unwrap();
        {
            let s = SqliteStore::open(p).unwrap();
            s.create("local", &NoteInput { body: "old note".into(), ..Default::default() }).unwrap();
            s.add_rule("rules-only", "x", "y").unwrap();
        }
        {
            let c = Connection::open(p).unwrap();
            c.execute_batch("DROP TABLE users;").unwrap(); // a database from before accounts existed
        }
        let s = SqliteStore::open(p).unwrap();
        let ids: Vec<String> = s.list_users().unwrap().into_iter().map(|u| u.user.id).collect();
        assert_eq!(ids, vec!["local".to_string(), "rules-only".to_string()]);
        drop(s);
        let _ = std::fs::remove_file(&path);
    }
}
