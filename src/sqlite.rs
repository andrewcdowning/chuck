use crate::store::*;
use crate::tagging::{feedback_offset, strip_hashtags, Doc, Model, Suggestion};
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
             CREATE INDEX IF NOT EXISTS suggestion_events_owner ON suggestion_events(owner, tag);",
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
        Ok(Self { conn: Mutex::new(conn), generation: AtomicU64::new(0), models: Mutex::new(HashMap::new()) })
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
        conn.execute(
            "INSERT INTO notes(owner, body, created_at, updated_at, remind_at, repeat, due_at, open_todos) VALUES(?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)",
            params![owner, body, t, remind_at, repeat, due_at, open_items(body)],
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
            sql.push_str(
                " ORDER BY (n.open_todos > 0) DESC,
                  CASE WHEN n.open_todos > 0 THEN COALESCE(n.due_at, 9223372036854775807) END ASC,
                  n.created_at DESC, n.id DESC LIMIT ? OFFSET ?",
            );
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

    fn suggest_tags(&self, owner: &str, text: &str) -> Result<Vec<Suggestion>> {
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
        Ok(model.suggest(text, &taken, &offsets))
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

    #[test]
    fn todo_cards_pin_and_due() {
        let s = SqliteStore::open(":memory:").unwrap();
        let mk = |b: &str, due: Option<i64>| NoteInput { body: b.into(), due_at: due, ..Default::default() };
        let plain_old = s.create("me", &mk("plain old", None)).unwrap();
        let todo_nodue = s.create("me", &mk("Chores\n- [ ] sweep", None)).unwrap();
        let todo_late = s.create("me", &mk("Later\n- [ ] x", Some(2_000))).unwrap();
        let todo_soon = s.create("me", &mk("Soon\n- [ ] y", Some(1_000))).unwrap();
        let plain_new = s.create("me", &mk("plain new", None)).unwrap();
        assert_eq!(todo_soon.kind, "todo");
        assert_eq!(todo_soon.due_at, Some(1_000));

        let ids = |s: &SqliteStore| s.list("me", &NoteQuery::default()).unwrap().into_iter().map(|n| n.id).collect::<Vec<_>>();
        // open todos first, soonest due first, undated after dated; then the rest newest first
        assert_eq!(ids(&s), vec![todo_soon.id, todo_late.id, todo_nodue.id, plain_new.id, plain_old.id]);

        // completing every item drops the card out of the pinned group
        s.update("me", todo_soon.id, &mk("Soon\n- [x] y", None)).unwrap();
        assert_eq!(ids(&s), vec![todo_late.id, todo_nodue.id, plain_new.id, todo_soon.id, plain_old.id]);
        // due date is kept when absent from an update, and cleared with 0
        assert_eq!(s.get("me", todo_soon.id).unwrap().due_at, Some(1_000));
        let cleared = s.update("me", todo_soon.id, &mk("Soon\n- [x] y", Some(0))).unwrap();
        assert!(cleared.due_at.is_none());
        // reopening an item pins it again
        s.update("me", todo_soon.id, &mk("Soon\n- [ ] y", None)).unwrap();
        assert_eq!(ids(&s), vec![todo_late.id, todo_soon.id, todo_nodue.id, plain_new.id, plain_old.id]);
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
    fn tag_suggestions_confidence_and_feedback() {
        let s = SqliteStore::open(":memory:").unwrap();
        let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
        for i in 0..20 {
            s.create("me", &inp(&format!("Deploy the lambda stack to staging, bucket policy v{i} #aws"))).unwrap();
            s.create("me", &inp(&format!("Buy milk and garden soil, plumber visit v{i} #home"))).unwrap();
        }
        s.create("other", &inp("lambda lambda lambda #secret")).unwrap();

        let sug = s.suggest_tags("me", "investigate lambda deploy timeout").unwrap();
        assert_eq!(sug[0].tag, "aws");
        assert!(sug[0].confidence >= 0.7 && sug[0].tier == "high", "{:?}", sug[0]);
        assert!(!sug[0].why.is_empty());
        assert!(sug.iter().all(|x| x.tag != "secret"));
        // written tags and rule-added tags are never suggested
        assert!(s.suggest_tags("me", "lambda #aws").unwrap().iter().all(|x| x.tag != "aws"));
        s.add_rule("me", "lambda", "aws").unwrap();
        assert!(s.suggest_tags("me", "lambda").unwrap().iter().all(|x| x.tag != "aws"));
        assert!(s.suggest_tags("me", "zzz qqq").unwrap().is_empty());

        // dismissals lower a tag's confidence
        let s2 = SqliteStore::open(":memory:").unwrap();
        for i in 0..20 {
            s2.create("me", &inp(&format!("Deploy the lambda stack to staging, bucket policy v{i} #aws"))).unwrap();
            s2.create("me", &inp(&format!("Buy milk and garden soil, plumber visit v{i} #home"))).unwrap();
        }
        let before = s2.suggest_tags("me", "lambda bucket").unwrap()[0].confidence;
        for _ in 0..6 {
            s2.record_suggestion_feedback("me", "aws", false, 0.9).unwrap();
        }
        let after = s2.suggest_tags("me", "lambda bucket").unwrap().iter().find(|x| x.tag == "aws").map_or(0.0, |x| x.confidence);
        assert!(after < before, "{after} !< {before}");

        // the cached model follows edits: a brand new tag becomes suggestable once notes use it
        for i in 0..4 {
            s2.create("me", &inp(&format!("Sourdough starter feeding schedule {i} #baking"))).unwrap();
        }
        assert!(s2.suggest_tags("me", "feed the sourdough starter").unwrap().iter().any(|x| x.tag == "baking"));
    }
}
