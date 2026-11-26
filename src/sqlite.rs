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
             CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(body);
             CREATE TABLE IF NOT EXISTS tag_rules(
               id INTEGER PRIMARY KEY,
               owner TEXT NOT NULL,
               pattern TEXT NOT NULL,
               tag TEXT NOT NULL,
               created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS tag_rules_owner ON tag_rules(owner);
             CREATE TABLE IF NOT EXISTS todo_items(
               id INTEGER PRIMARY KEY,
               owner TEXT NOT NULL,
               text TEXT NOT NULL,
               done INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS todo_items_owner ON todo_items(owner, done, id);",
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
                        kind: "note",
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
        note.kind = card_kind(&note.body, note.remind_at);
        Ok(note)
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

    fn todo_row(r: &rusqlite::Row, rules: &[(String, String)]) -> rusqlite::Result<TodoEntry> {
        let text: String = r.get(1)?;
        Ok(TodoEntry { id: r.get(0)?, tags: Self::tags_for(rules, &text), text, done: r.get::<_, i64>(2)? != 0, created_at: r.get(3)? })
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
        conn.execute(
            "INSERT INTO notes(owner, body, created_at, updated_at, remind_at, repeat) VALUES(?1, ?2, ?3, ?3, ?4, ?5)",
            params![owner, body, t, remind_at, repeat],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute("INSERT INTO notes_fts(rowid, body) VALUES(?1, ?2)", params![id, body])?;
        Self::set_tags(&conn, owner, id, body)?;
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
        Self::set_tags(&conn, owner, id, body)?;
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

    fn get(&self, owner: &str, id: i64) -> Result<Note> {
        let conn = self.conn.lock().unwrap();
        Self::load(&conn, owner, id)
    }

    fn todo_list(&self, owner: &str) -> Result<Vec<TodoEntry>> {
        let conn = self.conn.lock().unwrap();
        let rules = Self::rule_pairs(&conn, owner)?;
        let mut st = conn.prepare("SELECT id, text, done, created_at FROM todo_items WHERE owner=?1 ORDER BY done ASC, id ASC")?;
        let rows = st.query_map([owner], |r| Self::todo_row(r, &rules))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    fn todo_add(&self, owner: &str, text: &str) -> Result<TodoEntry> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM todo_items WHERE owner=?1", [owner], |r| r.get(0))?;
        if n >= 1000 {
            return Err(StoreError::Other("too many todos".into()));
        }
        let t = now();
        conn.execute("INSERT INTO todo_items(owner, text, created_at) VALUES(?1, ?2, ?3)", params![owner, text, t])?;
        let tags = Self::tags_for(&Self::rule_pairs(&conn, owner)?, text);
        Ok(TodoEntry { id: conn.last_insert_rowid(), text: text.to_string(), done: false, created_at: t, tags })
    }

    fn todo_update(&self, owner: &str, id: i64, text: Option<&str>, done: Option<bool>) -> Result<TodoEntry> {
        let conn = self.conn.lock().unwrap();
        let cur = conn
            .query_row(
                "SELECT text, done, created_at FROM todo_items WHERE id=?1 AND owner=?2",
                params![id, owner],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0, r.get::<_, i64>(2)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let text = text.map(str::to_string).unwrap_or(cur.0);
        let done = done.unwrap_or(cur.1);
        conn.execute("UPDATE todo_items SET text=?1, done=?2 WHERE id=?3 AND owner=?4", params![text, done as i64, id, owner])?;
        let tags = Self::tags_for(&Self::rule_pairs(&conn, owner)?, &text);
        Ok(TodoEntry { id, text, done, created_at: cur.2, tags })
    }

    fn todo_delete(&self, owner: &str, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM todo_items WHERE id=?1 AND owner=?2", params![id, owner])?;
        if n == 0 { Err(StoreError::NotFound) } else { Ok(()) }
    }

    fn todo_clear_done(&self, owner: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM todo_items WHERE owner=?1 AND done=1", [owner])?)
    }

    fn suggest_tags(&self, owner: &str, text: &str) -> Result<Vec<String>> {
        const STOP: [&str; 24] = [
            "the", "and", "for", "with", "that", "this", "from", "have", "has", "was", "were", "are", "but", "not", "you",
            "your", "about", "into", "need", "get", "got", "just", "then", "them",
        ];
        let conn = self.conn.lock().unwrap();
        let rules = Self::rule_pairs(&conn, owner)?;
        // Tags already written or about to be added by a rule are not suggestions.
        let taken = Self::tags_for(&rules, text);

        let mut words: Vec<String> = Vec::new();
        for w in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
            if w.chars().count() >= 3 && !STOP.contains(&w) && !words.iter().any(|x| x == w) {
                words.push(w.to_string());
            }
            if words.len() >= 12 {
                break;
            }
        }

        let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        // 1. The tag's own name appears in the text.
        let mut st = conn.prepare(
            "SELECT DISTINCT t.tag FROM note_tags t JOIN notes n ON n.id=t.note_id WHERE n.owner=?1",
        )?;
        let existing: Vec<String> = st.query_map([owner], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        for tag in &existing {
            if words.iter().any(|w| w == tag || (w.len() >= 4 && tag.len() >= 4 && (tag.starts_with(w.as_str()) || w.starts_with(tag.as_str())))) {
                *scores.entry(tag.clone()).or_default() += 1.5;
            }
        }
        // 2. Words that usually appear in notes carrying a tag.
        for w in &words {
            let Some(fq) = fts_query(w) else { continue };
            let df: i64 = conn.query_row(
                "SELECT COUNT(*) FROM notes n WHERE n.owner=?1 AND n.id IN (SELECT rowid FROM notes_fts WHERE notes_fts MATCH ?2)",
                params![owner, fq],
                |r| r.get(0),
            )?;
            if df == 0 {
                continue;
            }
            let weight = (df.min(2) as f64) / 2.0; // a word seen in one note only is weak evidence
            let mut st = conn.prepare(
                "SELECT t.tag, COUNT(*) FROM note_tags t JOIN notes n ON n.id=t.note_id
                 WHERE n.owner=?1 AND t.note_id IN (SELECT rowid FROM notes_fts WHERE notes_fts MATCH ?2) GROUP BY t.tag",
            )?;
            let rows: Vec<(String, i64)> = st
                .query_map(params![owner, fq], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (tag, cnt) in rows {
                *scores.entry(tag).or_default() += (cnt as f64 / df as f64) * weight;
            }
        }
        let mut ranked: Vec<(String, f64)> = scores.into_iter().filter(|(t, s)| *s >= 0.8 && !taken.contains(t)).collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        Ok(ranked.into_iter().take(5).map(|(t, _)| t).collect())
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

    fn tags(&self, owner: &str) -> Result<Vec<TagCount>> {
        let conn = self.conn.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT t.tag, COUNT(*) FROM note_tags t JOIN notes n ON n.id=t.note_id WHERE n.owner=?1 GROUP BY t.tag",
        )?;
        let mut counts: std::collections::HashMap<String, i64> = st
            .query_map([owner], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        // Tags on todo items count too, so a tag used only on todos still shows up as a filter.
        let rules = Self::rule_pairs(&conn, owner)?;
        let mut st = conn.prepare("SELECT id, text, done, created_at FROM todo_items WHERE owner=?1")?;
        let todos: Vec<TodoEntry> = st.query_map([owner], |r| Self::todo_row(r, &rules))?.collect::<std::result::Result<_, _>>()?;
        for t in todos {
            for tag in t.tags {
                *counts.entry(tag).or_default() += 1;
            }
        }
        let mut out: Vec<TagCount> = counts.into_iter().map(|(tag, count)| TagCount { tag, count }).collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.tag.cmp(&b.tag)));
        Ok(out)
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

    #[test]
    fn card_kinds() {
        let s = SqliteStore::open(":memory:").unwrap();
        let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
        assert_eq!(s.create("me", &inp("Groceries\n- [ ] milk")).unwrap().kind, "list");
        assert_eq!(s.create("me", &inp("plain")).unwrap().kind, "note");
        let r = NoteInput { body: "x".into(), remind_at: Some(5), ..Default::default() };
        assert_eq!(s.create("me", &r).unwrap().kind, "reminder");
    }

    #[test]
    fn standalone_todos() {
        let s = SqliteStore::open(":memory:").unwrap();
        let a = s.todo_add("me", "buy milk").unwrap();
        let b = s.todo_add("me", "call Bob").unwrap();
        s.todo_add("other", "not mine").unwrap();

        s.todo_update("me", a.id, None, Some(true)).unwrap();
        // open items first, done items last
        let l = s.todo_list("me").unwrap();
        assert_eq!(l.iter().map(|t| (t.text.as_str(), t.done)).collect::<Vec<_>>(), vec![("call Bob", false), ("buy milk", true)]);

        let e = s.todo_update("me", b.id, Some("call Bob back"), None).unwrap();
        assert_eq!((e.text.as_str(), e.done), ("call Bob back", false));
        assert!(matches!(s.todo_update("other", a.id, None, Some(false)), Err(StoreError::NotFound)));
        assert!(matches!(s.todo_delete("other", a.id), Err(StoreError::NotFound)));

        assert_eq!(s.todo_clear_done("me").unwrap(), 1);
        assert_eq!(s.todo_list("me").unwrap().len(), 1);
        assert_eq!(s.todo_list("other").unwrap().len(), 1);
        s.todo_delete("me", b.id).unwrap();
        assert!(s.todo_list("me").unwrap().is_empty());
    }

    #[test]
    fn todo_tags_and_filters() {
        let s = SqliteStore::open(":memory:").unwrap();
        s.add_rule("me", "jira", "work").unwrap();
        let a = s.todo_add("me", "file jira ticket #urgent").unwrap();
        s.todo_add("me", "buy milk #home").unwrap();
        s.todo_add("other", "x #home").unwrap();
        s.create("me", &NoteInput { body: "note about #home".into(), ..Default::default() }).unwrap();

        assert_eq!(a.tags, vec!["urgent", "work"]); // written tag plus rule tag
        let l = s.todo_list("me").unwrap();
        assert_eq!(l[1].tags, vec!["home"]);
        // an edit re-derives tags
        let u = s.todo_update("me", a.id, Some("file ticket #later"), None).unwrap();
        assert_eq!(u.tags, vec!["later"]);
        // tag counts combine notes and todos, per owner
        let counts: Vec<(String, i64)> = s.tags("me").unwrap().into_iter().map(|t| (t.tag, t.count)).collect();
        assert_eq!(counts, vec![("home".to_string(), 2), ("later".to_string(), 1)]);
    }

    #[test]
    fn auto_tag_rules() {
        let s = SqliteStore::open(":memory:").unwrap();
        let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
        let old = s.create("me", &inp("Review PR-42 before standup")).unwrap();
        assert!(old.tags.is_empty());
        s.add_rule("me", "jira, PR-", "work").unwrap();
        s.add_rule("other", "milk", "private").unwrap();

        // new notes get the rule tag, and explicit tags stay
        let n = s.create("me", &inp("Fix PR-7 #urgent")).unwrap();
        assert_eq!(n.tags, vec!["urgent", "work"]);
        // existing notes are tagged on demand
        assert_eq!(s.apply_rules("me").unwrap(), 1);
        assert_eq!(s.get("me", old.id).unwrap().tags, vec!["work"]);
        assert_eq!(s.apply_rules("me").unwrap(), 0);
        // owner isolation
        assert_eq!(s.rules("me").unwrap().len(), 1);
        assert!(s.create("me", &inp("milk")).unwrap().tags.is_empty());
        let r = s.rules("me").unwrap();
        assert!(matches!(s.delete_rule("other", r[0].id), Err(StoreError::NotFound)));
        s.delete_rule("me", r[0].id).unwrap();
        assert!(s.rules("me").unwrap().is_empty());
    }

    #[test]
    fn tag_suggestions() {
        let s = SqliteStore::open(":memory:").unwrap();
        let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
        s.create("me", &inp("Deploy the lambda to staging #aws")).unwrap();
        s.create("me", &inp("Lambda cold start numbers #aws #perf")).unwrap();
        s.create("me", &inp("Buy sourdough starter #home")).unwrap();
        s.create("other", &inp("lambda lambda #secret")).unwrap();

        let sug = s.suggest_tags("me", "investigate lambda timeout").unwrap();
        assert_eq!(sug.first().map(String::as_str), Some("aws"));
        assert!(!sug.contains(&"secret".to_string()));
        // tag name in text counts, but a tag already written is not suggested again
        assert!(s.suggest_tags("me", "moving the home server").unwrap().contains(&"home".to_string()));
        assert!(!s.suggest_tags("me", "lambda #aws").unwrap().contains(&"aws".to_string()));
        // a rule-added tag is not suggested either
        s.add_rule("me", "lambda", "aws").unwrap();
        assert!(!s.suggest_tags("me", "lambda").unwrap().contains(&"aws".to_string()));
        assert!(s.suggest_tags("me", "zzz qqq").unwrap().is_empty());
    }
}
