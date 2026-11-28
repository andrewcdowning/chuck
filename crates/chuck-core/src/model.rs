//! The plain types the planes share: cards, attachments, rules, accounts, queries, and the error every storage layer
//! returns. No behaviour beyond what is here; the rules are in `domain`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    /// Opaque text (see `domain::id`).
    pub id: String,
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
    pub id: String,
    pub note_id: String,
    /// Detected from the file's bytes.
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Audio length, as measured by the uploading browser (display only).
    pub duration_ms: Option<i64>,
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
    pub duration_ms: Option<i64>,
    pub name: Option<String>,
    pub blob_key: String,
    pub sha256: String,
}

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

/// A person using this Chuck. Created the first time their identity is seen.
#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: String,
    pub display_name: Option<String>,
    /// "user" or "admin". Admins manage accounts; they have no access to anyone's content.
    pub role: String,
    /// "active" or "disabled". A disabled account is refused on every request; its data stays until deleted.
    pub status: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

/// What an admin sees about an account: counts and sizes, never content.
#[derive(Debug, Clone, Serialize)]
pub struct UserSummary {
    #[serde(flatten)]
    pub user: User,
    pub notes: i64,
    pub attachments: i64,
    /// Total size of the account's stored files (previews not counted).
    pub storage_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedbackEvent {
    pub tag: String,
    pub action: String,
    pub confidence: f64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    pub id: String,
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
    /// The request does not make sense (for example moving a card across the open-todo group).
    Invalid(String),
    /// The request was fine but the data it touches is not in the state it needs: someone else changed or removed it
    /// meanwhile (a unique or foreign-key constraint refused the write), or the account is in a state that forbids the
    /// action. The caller may look again and retry. The message is safe to show.
    Conflict(String),
    /// Anything else: a fault of the storage, not of the request. Logged, and reported only as "internal error".
    Other(String),
}

/// What a database error means for the caller. Constraint violations that say "this collides with what is stored now"
/// are conflicts; everything else (a full disk, a locked or corrupt file, a CHECK or NOT NULL violation, which would be a
/// bug) stays `Other`. Matches the text SQLite gives for its constraint errors, because an error from another crate
/// cannot be inspected any other way from here; the SQLite backend pins the wording with tests against real errors.
/// A backend whose errors carry codes should return `StoreError::Conflict` itself.
pub fn classify_database_error(message: &str) -> StoreError {
    const COLLISIONS: [&str; 3] = ["UNIQUE constraint failed", "PRIMARY KEY constraint failed", "FOREIGN KEY constraint failed"];
    if COLLISIONS.iter().any(|c| message.contains(c)) {
        StoreError::Conflict("the data changed while saving; look again and retry".into())
    } else {
        StoreError::Other(message.to_string())
    }
}

impl<E: std::error::Error> From<E> for StoreError {
    fn from(e: E) -> Self {
        classify_database_error(&e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collisions_are_conflicts_and_faults_are_not() {
        for m in [
            "UNIQUE constraint failed: users.id",
            "PRIMARY KEY constraint failed",
            "FOREIGN KEY constraint failed",
        ] {
            assert!(matches!(classify_database_error(m), StoreError::Conflict(_)), "{m}");
        }
        for m in ["disk I/O error", "database is locked", "CHECK constraint failed: role", "NOT NULL constraint failed: notes.body", "no such table: x"] {
            assert!(matches!(classify_database_error(m), StoreError::Other(_)), "{m}");
        }
        let StoreError::Conflict(m) = classify_database_error("UNIQUE constraint failed: tag_rules.pattern") else { panic!() };
        assert!(!m.contains("tag_rules"), "table and column names are not shown to callers");
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;


pub use crate::domain::card::{card_kind, extract_tags, normalize_tag, open_items, parse_item, rule_matches};
pub use crate::domain::input::MAX_ATTACHMENTS_PER_NOTE;
pub use crate::domain::schedule::REPEATS;

/// Who a request is from: the owner id, plus a display name when the identity provider has one.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub id: String,
    pub name: Option<String>,
}
