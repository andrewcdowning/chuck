use crate::store::*;
use crate::tagging::{feedback_offset, strip_hashtags, Doc, Model, SuggestionSet, MIN_DOCS_TO_CALIBRATE};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SqliteStore {
    conn: Mutex<Connection>,
    /// Bumped whenever note text changes; cached tagging models are rebuilt when it moves.
    generation: AtomicU64,
    models: Mutex<HashMap<String, (u64, Arc<Model>)>>,
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
        Ok(Self { conn: Mutex::new(conn), generation: AtomicU64::new(0), models: Mutex::new(HashMap::new()) })
    }

    /// The owner's cards in display order: open todos first, then the rest, each group by position (highest first).
    fn ordered(conn: &Connection, owner: &str) -> Result<Vec<(i64, f64, bool, Option<i64>)>> {
        let mut st = conn.prepare(
            "SELECT n.id, n.position, n.open_todos > 0, n.due_at FROM notes n WHERE n.owner=?1 ORDER BY (n.open_todos > 0) DESC, n.position DESC, n.id DESC",
        )?;
        let v = st.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(v)
    }

    /// Spreads the owner's positions back to whole numbers, keeping the order. Needed only after very many moves
    /// into the same gap have used up floating-point precision.
    fn renumber(conn: &Connection, owner: &str) -> Result<()> {
        let all = Self::ordered(conn, owner)?;
        let n = all.len();
        for (k, (id, ..)) in all.iter().enumerate() {
            conn.execute("UPDATE notes SET position=?1 WHERE id=?2 AND owner=?3", params![(n - k) as f64, id, owner])?;
        }
        Ok(())
    }

    /// A position between two neighbours (either may be missing). None if they are too close to split.
    fn between(above: Option<f64>, below: Option<f64>) -> Option<f64> {
        let p = match (above, below) {
            (Some(a), Some(b)) => (a + b) / 2.0,
            (Some(a), None) => a - 1.0,
            (None, Some(b)) => b + 1.0,
            (None, None) => 1.0,
        };
        let ok = above.map_or(true, |a| a - p > 1e-9) && below.map_or(true, |b| p - b > 1e-9);
        ok.then_some(p)
    }

    /// Where a new card goes. Ordinary cards go on top. A new open todo is slotted by due date among the open todos
    /// as they are currently ordered: just above the first one due later (or undated); undated todos go last.
    fn new_position(conn: &Connection, owner: &str, open_todo: bool, due: Option<i64>) -> Result<f64> {
        let all = Self::ordered(conn, owner)?;
        let top = all.iter().map(|x| x.1).fold(0.0_f64, f64::max);
        if !open_todo {
            let rest_top = all.iter().filter(|x| !x.2).map(|x| x.1).fold(f64::MIN, f64::max);
            return Ok(if rest_top == f64::MIN { top + 1.0 } else { rest_top.max(top) + 1.0 });
        }
        let group: Vec<_> = all.iter().filter(|x| x.2).collect();
        if group.is_empty() {
            return Ok(top + 1.0);
        }
        let later = |d: Option<i64>| match (due, d) {
            (Some(a), Some(b)) => b > a,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let idx = group.iter().position(|x| later(x.3)).unwrap_or(group.len());
        let above = if idx == 0 { None } else { Some(group[idx - 1].1) };
        let below = group.get(idx).map(|x| x.1);
        match Self::between(above, below) {
            Some(p) => Ok(p),
            None => {
                Self::renumber(conn, owner)?;
                Self::new_position(conn, owner, open_todo, due)
            }
        }
    }

    /// Notes that carry at least one tag the user wrote, with the hashtags stripped from the text.
    /// Tags added by auto-tag rules are deliberately not labels.
    pub fn training_docs(&self, owner: &str) -> Result<Vec<Doc>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT body FROM notes WHERE owner=?1")?;
        let bodies: Vec<String> = st.query_map([owner], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(bodies
            .iter()
            .map(|b| Doc { labels: extract_tags(b), text: strip_hashtags(b) })
            .filter(|d| !d.labels.is_empty())
            .collect())
    }

    /// Owners with notes in this database, and how many each.
    pub fn owners(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT owner, COUNT(*) FROM notes GROUP BY owner ORDER BY owner")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn model(&self, owner: &str) -> Result<Arc<Model>> {
        let generation = self.generation.load(Ordering::Relaxed);
        if let Some((g, m)) = self.models.lock().unwrap().get(owner) {
            if *g == generation {
                return Ok(m.clone());
            }
        }
        let model = Arc::new(Model::train(&self.training_docs(owner)?));
        self.models.lock().unwrap().insert(owner.to_string(), (generation, model.clone()));
        Ok(model)
    }

    fn load(conn: &Connection, owner: &str, id: i64) -> Result<Note> {
        let mut note = conn
            .query_row(
                "SELECT id, body, created_at, updated_at, remind_at, repeat, reminder_done, due_at FROM notes WHERE id=?1 AND owner=?2",
                params![id, owner],
                |r| {
                    Ok(Note {
                        id: r.get(0)?,
                        kind: "note",
                        body: r.get(1)?,
                        tags: vec![],
                        created_at: r.get(2)?,
                        updated_at: r.get(3)?,
                        remind_at: r.get(4)?,
                        repeat: r.get(5)?,
                        reminder_done: r.get::<_, i64>(6)? != 0,
                        due_at: r.get(7)?,
                        attachments: vec![],
                    })
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let mut st = conn.prepare("SELECT tag FROM note_tags WHERE note_id=?1 ORDER BY tag")?;
        note.tags = st.query_map([id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        note.attachments = Self::note_attachments(conn, owner, id)?;
        note.kind = card_kind(&note.body, note.remind_at, !note.attachments.is_empty());
        Ok(note)
    }

    const ATTACHMENT_COLS: &'static str = "id, note_id, mime, bytes, width, height, name, blob_key, thumb_key, duration_ms";

    fn attachment_row(r: &rusqlite::Row) -> rusqlite::Result<Attachment> {
        let thumb_key: Option<String> = r.get(8)?;
        let blob_key: String = r.get(7)?;
        // Derived from the storage keys, which already include the owner, so two users' files never share a version.
        let v = crate::blob::sha256_hex(format!("{blob_key}|{}", thumb_key.as_deref().unwrap_or("")).as_bytes())[..16].to_string();
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
            v,
            blob_key,
            thumb_key,
        })
    }

    fn note_attachments(conn: &Connection, owner: &str, note_id: i64) -> Result<Vec<Attachment>> {
        let sql = format!("SELECT {} FROM attachments WHERE note_id=?1 AND owner=?2 ORDER BY position, id", Self::ATTACHMENT_COLS);
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(params![note_id, owner], Self::attachment_row)?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn load_attachment(conn: &Connection, owner: &str, id: i64) -> Result<Attachment> {
        let sql = format!("SELECT {} FROM attachments WHERE id=?1 AND owner=?2", Self::ATTACHMENT_COLS);
        conn.query_row(&sql, params![id, owner], Self::attachment_row).optional()?.ok_or(StoreError::NotFound)
    }

    fn rule_pairs(conn: &Connection, owner: &str) -> Result<Vec<(String, String)>> {
        let mut st = conn.prepare("SELECT pattern, tag FROM tag_rules WHERE owner=?1 ORDER BY id")?;
        let rows = st.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    /// Tags written in the text plus tags added by the owner's rules.
    fn tags_for(rules: &[(String, String)], body: &str) -> Vec<String> {
        let mut tags = extract_tags(body);
        let lower = body.to_lowercase();
        for (pattern, tag) in rules {
            if rule_matches(pattern, &lower) && !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        tags
    }

    fn set_tags(conn: &Connection, owner: &str, id: i64, body: &str) -> Result<()> {
        let rules = Self::rule_pairs(conn, owner)?;
        conn.execute("DELETE FROM note_tags WHERE note_id=?1", [id])?;
        for t in Self::tags_for(&rules, body) {
            conn.execute("INSERT OR IGNORE INTO note_tags(note_id, tag) VALUES(?1, ?2)", params![id, t])?;
        }
        Ok(())
    }
}

/// Turn user input into a safe FTS5 query: each word quoted, prefix-matched.
fn fts_query(q: &str) -> Option<String> {
    let parts: Vec<String> = q
        .split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"*", w))
        .collect();
    if parts.is_empty() { None } else { Some(parts.join(" ")) }
}

impl NoteStore for SqliteStore {
    fn create(&self, owner: &str, input: &NoteInput) -> Result<Note> {
        let body = input.body.as_str();
        let conn = self.conn.lock().unwrap();
        let t = now();
        let remind_at = input.remind_at.filter(|v| *v > 0);
        let repeat = input.repeat.clone().filter(|r| !r.is_empty());
        let due_at = input.due_at.filter(|v| *v > 0);
        let position = Self::new_position(&conn, owner, open_items(body) > 0, due_at)?;
        conn.execute(
            "INSERT INTO notes(owner, body, created_at, updated_at, remind_at, repeat, due_at, open_todos, position) VALUES(?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![owner, body, t, remind_at, repeat, due_at, open_items(body), position],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute("INSERT INTO notes_fts(rowid, body) VALUES(?1, ?2)", params![id, body])?;
        Self::set_tags(&conn, owner, id, body)?;
        self.generation.fetch_add(1, Ordering::Relaxed);
        Self::load(&conn, owner, id)
    }

    fn update(&self, owner: &str, id: i64, input: &NoteInput) -> Result<Note> {
        let body = input.body.as_str();
        let conn = self.conn.lock().unwrap();
        let cur = Self::load(&conn, owner, id)?;
        let remind_at = match input.remind_at {
            None => cur.remind_at,
            Some(v) if v <= 0 => None,
            Some(v) => Some(v),
        };
        let repeat = match &input.repeat {
            None => cur.repeat.clone(),
            Some(r) if r.is_empty() => None,
            Some(r) => Some(r.clone()),
        };
        let done = input
            .reminder_done
            .unwrap_or(if remind_at != cur.remind_at { false } else { cur.reminder_done });
        let due_at = match input.due_at {
            None => cur.due_at,
            Some(v) if v <= 0 => None,
            Some(v) => Some(v),
        };
        conn.execute(
            "UPDATE notes SET body=?1, updated_at=?2, remind_at=?3, repeat=?4, reminder_done=?5, due_at=?6, open_todos=?7 WHERE id=?8 AND owner=?9",
            params![body, now(), remind_at, repeat, done as i64, due_at, open_items(body), id, owner],
        )?;
        conn.execute("UPDATE notes_fts SET body=?1 WHERE rowid=?2", params![body, id])?;
        Self::set_tags(&conn, owner, id, body)?;
        self.generation.fetch_add(1, Ordering::Relaxed);
        Self::load(&conn, owner, id)
    }

    fn delete(&self, owner: &str, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM notes WHERE id=?1 AND owner=?2", params![id, owner])?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        conn.execute("DELETE FROM notes_fts WHERE rowid=?1", [id])?;
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn list(&self, owner: &str, q: &NoteQuery) -> Result<Vec<Note>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from("SELECT n.id FROM notes n WHERE n.owner=?");
        let mut args: Vec<rusqlite::types::Value> = vec![owner.to_string().into()];
        if let Some(fq) = q.q.as_deref().and_then(fts_query) {
            sql.push_str(" AND n.id IN (SELECT rowid FROM notes_fts WHERE notes_fts MATCH ?)");
            args.push(fq.into());
        }
        if let Some(tag) = q.tag.as_deref().filter(|t| !t.is_empty()) {
            sql.push_str(" AND n.id IN (SELECT note_id FROM note_tags WHERE tag=?)");
            args.push(tag.trim_start_matches('#').to_lowercase().into());
        }
        if let Some(from) = q.from {
            sql.push_str(" AND n.created_at>=?");
            args.push(from.into());
        }
        if let Some(to) = q.to {
            sql.push_str(" AND n.created_at<=?");
            args.push(to.into());
        }
        if q.reminders.unwrap_or(false) {
            sql.push_str(" AND n.remind_at IS NOT NULL AND n.reminder_done=0 ORDER BY n.remind_at ASC, n.id ASC LIMIT ? OFFSET ?");
        } else {
            // Todo cards with open items stay on top (soonest due first, undated last) until they are complete.
            sql.push_str(" ORDER BY (n.open_todos > 0) DESC, n.position DESC, n.id DESC LIMIT ? OFFSET ?");
        }
        args.push((q.limit.unwrap_or(50).min(200) as i64).into());
        args.push((q.offset.unwrap_or(0) as i64).into());

        let ids: Vec<i64> = conn
            .prepare(&sql)?
            .query_map(params_from_iter(args), |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        ids.into_iter().map(|id| Self::load(&conn, owner, id)).collect()
    }

    fn get(&self, owner: &str, id: i64) -> Result<Note> {
        let conn = self.conn.lock().unwrap();
        Self::load(&conn, owner, id)
    }

    fn move_note(&self, owner: &str, id: i64, prev: Option<i64>, next: Option<i64>) -> Result<Note> {
        {
            let conn = self.conn.lock().unwrap();
            let own: Option<i64> = conn.query_row("SELECT id FROM notes WHERE id=?1 AND owner=?2", params![id, owner], |r| r.get(0)).optional()?;
            if own.is_none() {
                return Err(StoreError::NotFound);
            }
        }
        if prev.is_none() && next.is_none() {
            return Err(StoreError::Invalid("give the card above or below the new place".into()));
        }
        if prev == Some(id) || next == Some(id) {
            return Err(StoreError::Invalid("a card cannot be its own neighbour".into()));
        }
        let conn = self.conn.lock().unwrap();
        let info = |nid: i64| -> Result<(f64, bool)> {
            conn.query_row("SELECT position, open_todos > 0 FROM notes WHERE id=?1 AND owner=?2", params![nid, owner], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or(StoreError::NotFound)
        };
        let (_, pinned) = info(id)?;
        let mut above = prev.map(info).transpose()?;
        let mut below = next.map(info).transpose()?;
        if above.iter().chain(below.iter()).any(|(_, p)| *p != pinned) {
            return Err(StoreError::Invalid("open todo cards stay above the other cards; move it within its group".into()));
        }
        if let (Some(a), Some(b)) = (above, below) {
            if a.0 < b.0 {
                return Err(StoreError::Invalid("the card above must come before the card below".into()));
            }
        }
        let pos = match Self::between(above.map(|x| x.0), below.map(|x| x.0)) {
            Some(p) => p,
            None => {
                Self::renumber(&conn, owner)?;
                above = prev.map(info).transpose()?;
                below = next.map(info).transpose()?;
                Self::between(above.map(|x| x.0), below.map(|x| x.0)).ok_or_else(|| StoreError::Other("could not place card".into()))?
            }
        };
        conn.execute("UPDATE notes SET position=?1 WHERE id=?2 AND owner=?3", params![pos, id, owner])?;
        Self::load(&conn, owner, id)
    }

    fn suggest_tags(&self, owner: &str, text: &str) -> Result<SuggestionSet> {
        let model = self.model(owner)?;
        let conn = self.conn.lock().unwrap();
        // Tags already written, or about to be added by a rule, are not suggestions.
        let taken = Self::tags_for(&Self::rule_pairs(&conn, owner)?, text);
        let mut st = conn.prepare(
            "SELECT tag, SUM(action='accepted'), SUM(action='dismissed') FROM suggestion_events WHERE owner=?1 GROUP BY tag",
        )?;
        let offsets: HashMap<String, f64> = st
            .query_map([owner], |r| {
                Ok((r.get::<_, String>(0)?, feedback_offset(r.get::<_, i64>(1)?.max(0) as u32, r.get::<_, i64>(2)?.max(0) as u32)))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(SuggestionSet {
            suggestions: model.suggest(text, &taken, &offsets),
            tagged_notes: model.docs,
            calibrated: model.calibrated,
            calibrate_at: MIN_DOCS_TO_CALIBRATE,
        })
    }

    fn record_suggestion_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO suggestion_events(owner, tag, action, confidence, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![owner, tag, if accepted { "accepted" } else { "dismissed" }, confidence.clamp(0.0, 1.0), now()],
        )?;
        Ok(())
    }

    fn rules(&self, owner: &str) -> Result<Vec<Rule>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT id, pattern, tag FROM tag_rules WHERE owner=?1 ORDER BY id")?;
        let rows = st
            .query_map([owner], |r| Ok(Rule { id: r.get(0)?, pattern: r.get(1)?, tag: r.get(2)? }))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn add_rule(&self, owner: &str, pattern: &str, tag: &str) -> Result<Rule> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM tag_rules WHERE owner=?1", [owner], |r| r.get(0))?;
        if n >= 200 {
            return Err(StoreError::Other("too many rules".into()));
        }
        conn.execute(
            "INSERT INTO tag_rules(owner, pattern, tag, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![owner, pattern, tag, now()],
        )?;
        Ok(Rule { id: conn.last_insert_rowid(), pattern: pattern.to_string(), tag: tag.to_string() })
    }

    fn delete_rule(&self, owner: &str, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM tag_rules WHERE id=?1 AND owner=?2", params![id, owner])?;
        if n == 0 { Err(StoreError::NotFound) } else { Ok(()) }
    }

    fn apply_rules(&self, owner: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let rules = Self::rule_pairs(&conn, owner)?;
        let mut st = conn.prepare("SELECT id, body FROM notes WHERE owner=?1")?;
        let notes: Vec<(i64, String)> = st
            .query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut changed = 0;
        for (id, body) in notes {
            let mut before: Vec<String> = Self::load(&conn, owner, id)?.tags;
            let mut after = Self::tags_for(&rules, &body);
            before.sort();
            after.sort();
            if before != after {
                Self::set_tags(&conn, owner, id, &body)?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    fn add_attachment(&self, owner: &str, note_id: i64, a: &NewAttachment) -> Result<Attachment> {
        let conn = self.conn.lock().unwrap();
        let mine: i64 = conn.query_row("SELECT COUNT(*) FROM notes WHERE id=?1 AND owner=?2", params![note_id, owner], |r| r.get(0))?;
        if mine == 0 {
            return Err(StoreError::NotFound);
        }
        let (count, next): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(position), -1) + 1 FROM attachments WHERE note_id=?1 AND owner=?2",
            params![note_id, owner],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if count >= MAX_ATTACHMENTS_PER_NOTE {
            return Err(StoreError::Invalid("too many attachments on this card".into()));
        }
        conn.execute(
            "INSERT INTO attachments(owner, note_id, position, blob_key, sha256, mime, bytes, width, height, name, created_at, duration_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![owner, note_id, next, a.blob_key, a.sha256, a.mime, a.bytes, a.width, a.height, a.name, now(), a.duration_ms],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute("UPDATE notes SET updated_at=?1 WHERE id=?2 AND owner=?3", params![now(), note_id, owner])?;
        Self::load_attachment(&conn, owner, id)
    }

    fn attachment(&self, owner: &str, id: i64) -> Result<Attachment> {
        let conn = self.conn.lock().unwrap();
        Self::load_attachment(&conn, owner, id)
    }

    fn set_attachment_thumb(&self, owner: &str, id: i64, thumb_key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let old = Self::load_attachment(&conn, owner, id)?.thumb_key;
        conn.execute("UPDATE attachments SET thumb_key=?1 WHERE id=?2 AND owner=?3", params![thumb_key, id, owner])?;
        Ok(old.filter(|k| k != thumb_key))
    }

    fn delete_attachment(&self, owner: &str, id: i64) -> Result<Attachment> {
        let conn = self.conn.lock().unwrap();
        let a = Self::load_attachment(&conn, owner, id)?;
        conn.execute("DELETE FROM attachments WHERE id=?1 AND owner=?2", params![id, owner])?;
        Ok(a)
    }

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
        self.models.lock().unwrap().remove(id);
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(keys)
    }

    fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare("SELECT tag, action, confidence, created_at FROM suggestion_events WHERE owner=?1 ORDER BY id")?;
        let rows = st
            .query_map([owner], |r| Ok(FeedbackEvent { tag: r.get(0)?, action: r.get(1)?, confidence: r.get(2)?, created_at: r.get(3)? }))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn blob_referenced(&self, owner: &str, key: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM attachments WHERE owner=?1 AND (blob_key=?2 OR thumb_key=?2)",
            params![owner, key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn tags(&self, owner: &str) -> Result<Vec<TagCount>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT t.tag, COUNT(*) c FROM note_tags t JOIN notes n ON n.id=t.note_id
             WHERE n.owner=?1 GROUP BY t.tag ORDER BY c DESC, t.tag",
        )?;
        let rows = st
            .query_map([owner], |r| Ok(TagCount { tag: r.get(0)?, count: r.get(1)? }))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
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
