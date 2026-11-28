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

/// Synchronous on purpose: callers run it inside `spawn_blocking`.
/// A network-backed store can use its own runtime handle internally.
pub trait NoteStore: Send + Sync + 'static {
    fn create(&self, owner: &str, input: &NoteInput) -> Result<Note>;
    fn update(&self, owner: &str, id: i64, input: &NoteInput) -> Result<Note>;
    fn delete(&self, owner: &str, id: i64) -> Result<()>;
    fn list(&self, owner: &str, q: &NoteQuery) -> Result<Vec<Note>>;
    fn tags(&self, owner: &str) -> Result<Vec<TagCount>>;
    fn get(&self, owner: &str, id: i64) -> Result<Note>;
    /// Moves a card to sit between two neighbours as the user sees them: `prev` directly above it, `next` directly
    /// below. One of them may be None (top or bottom). Open todo cards stay in their own group above the rest, so
    /// neighbours must be in the card's group.
    fn move_note(&self, owner: &str, id: i64, prev: Option<i64>, next: Option<i64>) -> Result<Note>;
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

    // ---- accounts
    /// Called on every authenticated request. Creates the account on first sight, keeps the role in step with the
    /// configured admin list, remembers the display name, and records activity (at most every few minutes).
    fn touch_user(&self, id: &str, display_name: Option<&str>, admin: bool) -> Result<User>;
    fn list_users(&self) -> Result<Vec<UserSummary>>;
    fn set_user_active(&self, id: &str, active: bool) -> Result<User>;
    /// Removes the account and everything it owns. Returns the storage keys of its files so the caller can delete them.
    fn delete_user(&self, id: &str) -> Result<Vec<String>>;
    /// The owner's accepted and dismissed tag suggestions, oldest first (part of their own data export).
    fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>>;
}

pub use crate::domain::card::{card_kind, extract_tags, normalize_tag, open_items, parse_item, rule_matches};
pub use crate::domain::input::MAX_ATTACHMENTS_PER_NOTE;
pub use crate::domain::schedule::REPEATS;
