//! Storage abstraction. The HTTP layer only talks to `NoteStore`, so a hosted
//! deployment can add a Postgres/DynamoDB implementation without touching `api.rs`.

use crate::tagging::SuggestionSet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    /// Card kind, derived from content: "reminder", "todo" (has checklist items), "media" (has attachments) or "note".
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
    /// Due date for todo cards (unix seconds, chosen by the client, usually end of the local day).
    pub due_at: Option<i64>,
    /// Pictures (and later audio) on the card, in display order.
    pub attachments: Vec<Attachment>,
}

/// A file attached to a note. The storage keys are never sent to clients; files are served by id after an owner check.
#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub id: i64,
    pub note_id: i64,
    /// Detected from the file's bytes.
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Original file name, for display only.
    pub name: Option<String>,
    pub has_thumb: bool,
    /// Changes whenever the stored file or its preview changes. Clients put it in the media URL so a browser never
    /// shows a cached picture for an id that has been reused (SQLite reuses the highest id after a delete).
    pub v: String,
    #[serde(skip)]
    pub blob_key: String,
    #[serde(skip)]
    pub thumb_key: Option<String>,
}

pub struct NewAttachment {
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub name: Option<String>,
    pub blob_key: String,
    pub sha256: String,
}

/// Most attachments one card can hold.
pub const MAX_ATTACHMENTS_PER_NOTE: i64 = 50;

/// Create/update payload. On update, an absent field means "unchanged";
/// `remind_at <= 0`, `due_at <= 0` or `repeat == ""` clears it. Changing `remind_at` re-arms the reminder.
#[derive(Debug, Default, Deserialize)]
pub struct NoteInput {
    pub body: String,
    pub remind_at: Option<i64>,
    pub repeat: Option<String>,
    pub reminder_done: Option<bool>,
    pub due_at: Option<i64>,
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
    /// Tags the user already has that fit `text`, best first, each with a calibrated confidence. Local, no external calls.
    /// The set also says how many tagged notes the model learned from, so an empty result can be explained.
    fn suggest_tags(&self, owner: &str, text: &str) -> Result<SuggestionSet>;
    /// Logs that a suggestion was accepted or dismissed; this adjusts that tag's future confidence.
    fn record_suggestion_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64) -> Result<()>;
    fn rules(&self, owner: &str) -> Result<Vec<Rule>>;
    fn add_rule(&self, owner: &str, pattern: &str, tag: &str) -> Result<Rule>;
    fn delete_rule(&self, owner: &str, id: i64) -> Result<()>;
    /// Re-tags every note with the current rules. Returns how many notes changed.
    fn apply_rules(&self, owner: &str) -> Result<usize>;

    /// Attaches a stored file to one of the owner's notes. NotFound if the note is not theirs.
    fn add_attachment(&self, owner: &str, note_id: i64, a: &NewAttachment) -> Result<Attachment>;
    fn attachment(&self, owner: &str, id: i64) -> Result<Attachment>;
    /// Sets the preview image. Returns the key of the preview it replaced, if any, so the caller can clean it up.
    fn set_attachment_thumb(&self, owner: &str, id: i64, thumb_key: &str) -> Result<Option<String>>;
    /// Removes an attachment and returns it, so the caller can clean up its files.
    fn delete_attachment(&self, owner: &str, id: i64) -> Result<Attachment>;
    /// True if any of the owner's attachments still uses this storage key (as a file or a preview).
    fn blob_referenced(&self, owner: &str, key: &str) -> Result<bool>;

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

/// Number of unchecked checklist items in the body.
pub fn open_items(body: &str) -> i64 {
    body.lines().filter(|l| matches!(parse_item(l), Some((false, _)))).count() as i64
}

pub fn card_kind(body: &str, remind_at: Option<i64>, has_media: bool) -> &'static str {
    if remind_at.is_some() {
        "reminder"
    } else if body.lines().any(|l| parse_item(l).is_some()) {
        "todo"
    } else if has_media {
        "media"
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
    fn checklist_items() {
        assert_eq!(parse_item("- [ ] milk"), Some((false, "milk")));
        assert_eq!(parse_item("  * [x] eggs"), Some((true, "eggs")));
        assert_eq!(parse_item("- [X]done"), Some((true, "done")));
        assert!(parse_item("- [y] no").is_none());
        assert!(parse_item("text [ ] x").is_none());
        assert_eq!(open_items("Groceries\n- [ ] milk\n- [x] eggs\n* [ ] bread\ntext [ ] x"), 2);
        assert_eq!(open_items("nothing"), 0);
    }

    #[test]
    fn rules_kinds_and_tag_names() {
        assert!(rule_matches("jira, PR-|sprint", "reviewing pr-123 today"));
        assert!(!rule_matches("jira", "nothing here"));
        assert!(!rule_matches(" , |", "anything"));
        assert_eq!(normalize_tag("#Work-1").as_deref(), Some("work-1"));
        assert!(normalize_tag("a b").is_none());
        assert!(normalize_tag("").is_none());
        assert_eq!(card_kind("- [ ] a", None, false), "todo");
        assert_eq!(card_kind("x", Some(5), true), "reminder");
        assert_eq!(card_kind("- [ ] a", None, true), "todo");
        assert_eq!(card_kind("x", None, true), "media");
        assert_eq!(card_kind("x", None, false), "note");
    }
}
