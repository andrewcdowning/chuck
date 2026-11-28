//! The plain types the planes share: cards, attachments, rules, accounts, queries, and the error every storage layer
//! returns. No behaviour beyond what is here; the rules are in `domain`.

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
    /// The request does not make sense (for example moving a card across the open-todo group).
    Invalid(String),
    Other(String),
}

impl<E: std::error::Error> From<E> for StoreError {
    fn from(e: E) -> Self {
        StoreError::Other(e.to_string())
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
