//! The data plane's storage interface as the HTTP layer sees it: cards, tags, rules, attachments and tag suggestions.
//! The plain types live in `chuck_core::model` and are re-exported here, so `store::Note` and friends keep working.

pub use chuck_core::model::*;
pub use chuck_core::tagging::SuggestionSet;

/// Synchronous on purpose: callers run it inside `spawn_blocking`.
/// A network-backed store can use its own runtime handle internally.
pub trait NoteStore: Send + Sync + 'static {
    fn create(&self, owner: &str, input: &NoteInput) -> Result<Note>;
    fn update(&self, owner: &str, id: &str, input: &NoteInput) -> Result<Note>;
    fn delete(&self, owner: &str, id: &str) -> Result<()>;
    fn list(&self, owner: &str, q: &NoteQuery) -> Result<Vec<Note>>;
    fn tags(&self, owner: &str) -> Result<Vec<TagCount>>;
    fn get(&self, owner: &str, id: &str) -> Result<Note>;
    /// Moves a card to sit between two neighbours as the user sees them: `prev` directly above it, `next` directly
    /// below. One of them may be None (top or bottom). Open todo cards stay in their own group above the rest, so
    /// neighbours must be in the card's group.
    fn move_note(&self, owner: &str, id: &str, prev: Option<&str>, next: Option<&str>) -> Result<Note>;
    /// Tags the user already has that fit `text`, best first, each with a calibrated confidence. Local, no external calls.
    /// The set also says how many tagged notes the model learned from, so an empty result can be explained.
    fn suggest_tags(&self, owner: &str, text: &str) -> Result<SuggestionSet>;
    /// Logs that a suggestion was accepted or dismissed; this adjusts that tag's future confidence.
    fn record_suggestion_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64) -> Result<()>;
    fn rules(&self, owner: &str) -> Result<Vec<Rule>>;
    fn add_rule(&self, owner: &str, pattern: &str, tag: &str) -> Result<Rule>;
    fn delete_rule(&self, owner: &str, id: &str) -> Result<()>;
    /// Re-tags every note with the current rules. Returns how many notes changed.
    fn apply_rules(&self, owner: &str) -> Result<usize>;

    /// Attaches a stored file to one of the owner's notes. NotFound if the note is not theirs.
    fn add_attachment(&self, owner: &str, note_id: &str, a: &NewAttachment) -> Result<Attachment>;
    fn attachment(&self, owner: &str, id: &str) -> Result<Attachment>;
    /// Sets the preview image. Returns the key of the preview it replaced, if any, so the caller can clean it up.
    fn set_attachment_thumb(&self, owner: &str, id: &str, thumb_key: &str) -> Result<Option<String>>;
    /// Removes an attachment and returns it, so the caller can clean up its files.
    fn delete_attachment(&self, owner: &str, id: &str) -> Result<Attachment>;
    /// True if any of the owner's attachments still uses this storage key (as a file or a preview).
    fn blob_referenced(&self, owner: &str, key: &str) -> Result<bool>;

    /// The owner's accepted and dismissed tag suggestions, oldest first (part of their own data export).
    fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>>;
}
