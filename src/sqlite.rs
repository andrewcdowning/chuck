use crate::store::*;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SqliteStore {
    conn: Mutex<Connection>,
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
             CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(body);",
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
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn load(conn: &Connection, owner: &str, id: i64) -> Result<Note> {
        let mut note = conn
            .query_row(
                "SELECT id, body, created_at, updated_at, remind_at, repeat, reminder_done FROM notes WHERE id=?1 AND owner=?2",
                params![id, owner],
                |r| {
                    Ok(Note {
                        id: r.get(0)?,
                        body: r.get(1)?,
                        tags: vec![],
                        created_at: r.get(2)?,
                        updated_at: r.get(3)?,
                        remind_at: r.get(4)?,
                        repeat: r.get(5)?,
                        reminder_done: r.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let mut st = conn.prepare("SELECT tag FROM note_tags WHERE note_id=?1 ORDER BY tag")?;
        note.tags = st.query_map([id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(note)
    }

    fn set_tags(conn: &Connection, id: i64, body: &str) -> Result<()> {
        conn.execute("DELETE FROM note_tags WHERE note_id=?1", [id])?;
        for t in extract_tags(body) {
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
        conn.execute(
            "INSERT INTO notes(owner, body, created_at, updated_at, remind_at, repeat) VALUES(?1, ?2, ?3, ?3, ?4, ?5)",
            params![owner, body, t, remind_at, repeat],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute("INSERT INTO notes_fts(rowid, body) VALUES(?1, ?2)", params![id, body])?;
        Self::set_tags(&conn, id, body)?;
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
        conn.execute(
            "UPDATE notes SET body=?1, updated_at=?2, remind_at=?3, repeat=?4, reminder_done=?5 WHERE id=?6 AND owner=?7",
            params![body, now(), remind_at, repeat, done as i64, id, owner],
        )?;
        conn.execute("UPDATE notes_fts SET body=?1 WHERE rowid=?2", params![body, id])?;
        Self::set_tags(&conn, id, body)?;
        Self::load(&conn, owner, id)
    }

    fn delete(&self, owner: &str, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM notes WHERE id=?1 AND owner=?2", params![id, owner])?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        conn.execute("DELETE FROM notes_fts WHERE rowid=?1", [id])?;
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
            sql.push_str(" ORDER BY n.created_at DESC, n.id DESC LIMIT ? OFFSET ?");
        }
        args.push((q.limit.unwrap_or(50).min(200) as i64).into());
        args.push((q.offset.unwrap_or(0) as i64).into());

        let ids: Vec<i64> = conn
            .prepare(&sql)?
            .query_map(params_from_iter(args), |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        ids.into_iter().map(|id| Self::load(&conn, owner, id)).collect()
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
    fn crud_search_tags_and_isolation() {
        let s = SqliteStore::open(":memory:").unwrap();
        let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
        let a = s.create("me", &inp("Call Bob about #project-x deadline")).unwrap();
        s.create("me", &inp("Buy milk #home")).unwrap();
        s.create("other", &inp("Secret #project-x")).unwrap();

        assert_eq!(a.tags, vec!["project-x"]);
        let r = s.list("me", &NoteQuery { q: Some("dead".into()), ..Default::default() }).unwrap();
        assert_eq!(r.len(), 1);
        let r = s.list("me", &NoteQuery { tag: Some("#HOME".into()), ..Default::default() }).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(s.tags("me").unwrap().len(), 2);

        let u = s.update("me", a.id, &inp("Now about #other")).unwrap();
        assert_eq!(u.tags, vec!["other"]);
        assert!(s.list("me", &NoteQuery { q: Some("deadline".into()), ..Default::default() }).unwrap().is_empty());
        assert!(matches!(s.delete("other", a.id), Err(StoreError::NotFound)));
        s.delete("me", a.id).unwrap();
        // injection-ish input must not error
        s.list("me", &NoteQuery { q: Some("\" OR * ( NEAR".into()), ..Default::default() }).unwrap();
    }

    #[test]
    fn reminders() {
        let s = SqliteStore::open(":memory:").unwrap();
        let n = s.create("me", &NoteInput { body: "Stand up".into(), remind_at: Some(2000), repeat: Some("daily".into()), ..Default::default() }).unwrap();
        s.create("me", &NoteInput { body: "later".into(), remind_at: Some(1000), ..Default::default() }).unwrap();
        s.create("me", &NoteInput { body: "plain".into(), ..Default::default() }).unwrap();
        let q = NoteQuery { reminders: Some(true), ..Default::default() };
        let r = s.list("me", &q).unwrap();
        assert_eq!(r.iter().map(|n| n.remind_at.unwrap()).collect::<Vec<_>>(), vec![1000, 2000]);

        // body-only edit leaves the reminder alone
        let u = s.update("me", n.id, &NoteInput { body: "Stand up!".into(), ..Default::default() }).unwrap();
        assert_eq!((u.remind_at, u.repeat.as_deref()), (Some(2000), Some("daily")));
        // marking done removes it from pending; re-arming with a new time brings it back
        s.update("me", n.id, &NoteInput { body: "Stand up!".into(), reminder_done: Some(true), ..Default::default() }).unwrap();
        assert_eq!(s.list("me", &q).unwrap().len(), 1);
        s.update("me", n.id, &NoteInput { body: "Stand up!".into(), remind_at: Some(3000), ..Default::default() }).unwrap();
        assert_eq!(s.list("me", &q).unwrap().len(), 2);
        // clearing
        let c = s.update("me", n.id, &NoteInput { body: "x".into(), remind_at: Some(0), repeat: Some("".into()), ..Default::default() }).unwrap();
        assert!(c.remind_at.is_none() && c.repeat.is_none());
    }
}
