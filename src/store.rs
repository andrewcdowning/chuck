//! Storage abstraction. The HTTP layer only talks to `NoteStore`, so a hosted
//! deployment can add a Postgres/DynamoDB implementation without touching `api.rs`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    /// Card kind, derived from content: "reminder", "list" or "note".
    pub kind: &'static str,
    pub body: String,
    pub tags: Vec<String>,
    pub created_at: i64, // unix seconds
    pub updated_at: i64,
    /// Next time the reminder is due (unix seconds), if this note is a reminder.
    pub remind_at: Option<i64>,
    /// "daily" | "weekdays" | "weekly" | "monthly". The client computes the next occurrence.
    pub repeat: Option<String>,
    pub reminder_done: bool,
}

/// Create/update payload. On update, an absent field means "unchanged";
/// `remind_at <= 0` or `repeat == ""` clears it. Changing `remind_at` re-arms the reminder.
#[derive(Debug, Default, Deserialize)]
pub struct NoteInput {
    pub body: String,
    pub remind_at: Option<i64>,
    pub repeat: Option<String>,
    pub reminder_done: Option<bool>,
}

pub const REPEATS: [&str; 4] = ["daily", "weekdays", "weekly", "monthly"];

#[derive(Debug, Default, Deserialize)]
pub struct NoteQuery {
    /// Free-text search (prefix match on each word).
    pub q: Option<String>,
    pub tag: Option<String>,
    /// Inclusive range on `created_at`, unix seconds.
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// Only pending reminders, soonest first.
    pub reminders: Option<bool>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TagCount {
    pub tag: String,
    pub count: i64,
}

/// A virtual card: an open checklist item pulled from some note.
#[derive(Debug, Serialize)]
pub struct TodoItem {
    pub note_id: i64,
    /// Zero-based line index within the note body.
    pub line: usize,
    pub text: String,
    /// First non-item line of the source note, as a label.
    pub source: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Rule {
    pub id: i64,
    /// Keywords separated by `|` or `,`; the rule fires if any appears (case-insensitive substring).
    pub pattern: String,
    pub tag: String,
}

#[derive(Debug, Deserialize)]
pub struct RuleInput {
    pub pattern: String,
    pub tag: String,
}

#[derive(Debug)]
pub enum StoreError {
    NotFound,
    Other(String),
}

impl<E: std::error::Error> From<E> for StoreError {
    fn from(e: E) -> Self {
        StoreError::Other(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Synchronous on purpose: callers run it inside `spawn_blocking`.
/// A network-backed store can use its own runtime handle internally.
pub trait NoteStore: Send + Sync + 'static {
    fn create(&self, owner: &str, input: &NoteInput) -> Result<Note>;
    fn update(&self, owner: &str, id: i64, input: &NoteInput) -> Result<Note>;
    fn delete(&self, owner: &str, id: i64) -> Result<()>;
    fn list(&self, owner: &str, q: &NoteQuery) -> Result<Vec<Note>>;
    fn tags(&self, owner: &str) -> Result<Vec<TagCount>>;
    fn get(&self, owner: &str, id: i64) -> Result<Note>;
    /// Open checklist items across all notes, newest note first.
    fn todos(&self, owner: &str) -> Result<Vec<TodoItem>>;
    /// Tags the user already has that fit `text`, best first. Local heuristics, no external calls.
    fn suggest_tags(&self, owner: &str, text: &str) -> Result<Vec<String>>;
    fn rules(&self, owner: &str) -> Result<Vec<Rule>>;
    fn add_rule(&self, owner: &str, pattern: &str, tag: &str) -> Result<Rule>;
    fn delete_rule(&self, owner: &str, id: i64) -> Result<()>;
    /// Re-tags every note with the current rules. Returns how many notes changed.
    fn apply_rules(&self, owner: &str) -> Result<usize>;

    /// Flip one checklist line. Provided in terms of `get` and `update`.
    fn toggle_item(&self, owner: &str, id: i64, line: usize) -> Result<Note> {
        let note = self.get(owner, id)?;
        let body = toggle_line(&note.body, line).ok_or(StoreError::NotFound)?;
        self.update(owner, id, &NoteInput { body, ..Default::default() })
    }
}

/// Parses `- [ ] text`, `* [x] text` (leading whitespace allowed). Returns (done, text).
pub fn parse_item(line: &str) -> Option<(bool, &str)> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* "))?.trim_start();
    let rest = rest.strip_prefix('[')?;
    let mut chars = rest.chars();
    let mark = chars.next()?;
    if chars.next()? != ']' || !matches!(mark, ' ' | 'x' | 'X') {
        return None;
    }
    let text = chars.as_str();
    let text = text.strip_prefix(' ').unwrap_or(text);
    Some((mark != ' ', text))
}

/// Returns the body with the checkbox on `line` flipped, or None if that line is not a checklist item.
pub fn toggle_line(body: &str, line: usize) -> Option<String> {
    let mut lines: Vec<String> = body.split('\n').map(String::from).collect();
    let l = lines.get_mut(line)?;
    let (done, _) = parse_item(l)?;
    let open = l.find('[')?;
    l.replace_range(open + 1..open + 2, if done { " " } else { "x" });
    Some(lines.join("\n"))
}

pub fn card_kind(body: &str, remind_at: Option<i64>) -> &'static str {
    if remind_at.is_some() {
        "reminder"
    } else if body.lines().any(|l| parse_item(l).is_some()) {
        "list"
    } else {
        "note"
    }
}

/// Tag names: letters, digits, `_` and `-`, lowercase, at most 40 characters.
pub fn normalize_tag(s: &str) -> Option<String> {
    let t = s.trim().trim_start_matches('#').to_lowercase();
    let ok = !t.is_empty() && t.chars().count() <= 40 && t.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    ok.then_some(t)
}

/// True if any keyword of the rule appears in the (already lowercased) text.
pub fn rule_matches(pattern: &str, text_lower: &str) -> bool {
    pattern
        .split(|c| c == '|' || c == ',')
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty())
        .any(|k| text_lower.contains(&k))
}

/// Extract `#tags` from text: lowercase, alphanumeric plus `_` and `-`, deduplicated.
pub fn extract_tags(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || chars[i - 1].is_whitespace();
        if chars[i] == '#' && at_boundary {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '-') {
                j += 1;
            }
            if j > i + 1 {
                let t: String = chars[i + 1..j].iter().collect::<String>().to_lowercase();
                if !out.contains(&t) {
                    out.push(t);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(extract_tags("buy milk #Home #home #to-do"), vec!["home", "to-do"]);
        assert!(extract_tags("issue#12 and # alone").is_empty());
    }

    #[test]
    fn items_and_toggle() {
        assert_eq!(parse_item("- [ ] milk"), Some((false, "milk")));
        assert_eq!(parse_item("  * [x] eggs"), Some((true, "eggs")));
        assert_eq!(parse_item("- [X]done"), Some((true, "done")));
        assert!(parse_item("- [y] no").is_none());
        assert!(parse_item("text [ ] x").is_none());
        let body = "Groceries\n- [ ] milk\n- [x] eggs";
        assert_eq!(toggle_line(body, 1).unwrap(), "Groceries\n- [x] milk\n- [x] eggs");
        assert_eq!(toggle_line(body, 2).unwrap(), "Groceries\n- [ ] milk\n- [ ] eggs");
        assert!(toggle_line(body, 0).is_none());
        assert!(toggle_line(body, 9).is_none());
    }

    #[test]
    fn rules_kinds_and_tag_names() {
        assert!(rule_matches("jira, PR-|sprint", "reviewing pr-123 today"));
        assert!(!rule_matches("jira", "nothing here"));
        assert!(!rule_matches(" , |", "anything"));
        assert_eq!(normalize_tag("#Work-1").as_deref(), Some("work-1"));
        assert!(normalize_tag("a b").is_none());
        assert!(normalize_tag("").is_none());
        assert_eq!(card_kind("- [ ] a", None), "list");
        assert_eq!(card_kind("x", Some(5)), "reminder");
        assert_eq!(card_kind("x", None), "note");
    }
}
