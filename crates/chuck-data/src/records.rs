//! The thin storage interface under the data services. A backend implements these traits and nothing else about notes:
//! every rule (what an update means, card kind, ordering, search terms, limits, tags from rules) lives in
//! `service::NoteService`, so all backends behave alike.
//!
//! The unit of work is `NoteRecords::transact`: the service reads and writes through a `RecordTx` and the backend makes
//! the whole closure atomic (a SQL transaction, a transactional write batch, or a swap of an in-memory copy). If the
//! closure returns an error nothing it wrote is kept.
//!
//! Every method takes the owner and must behave as if other owners' records do not exist: an id that belongs to someone
//! else is the same as an id that does not exist.

use chuck_core::domain::order::Slot;
use chuck_core::domain::schedule::Schedule;
use crate::blob::BlobRef;
use crate::store::{Attachment, NewAttachment, Result, Rule, TagCount};

/// A card as stored. `open_todos` and `position` are what ordering needs; the service computes and maintains them.
#[derive(Debug, Clone)]
pub struct NoteRecord {
    /// Chosen by the service (`ids::new_id`); a backend stores it as given and never makes ids of its own.
    pub id: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub schedule: Schedule,
    /// Unchecked checklist items. More than 0 puts the card in the top group.
    pub open_todos: i64,
    /// A string that sorts in display order (see `chuck_core::domain::order`): the highest is shown first.
    pub position: String,
    /// Sorted by name.
    pub tags: Vec<String>,
    /// In display order.
    pub attachments: Vec<AttachmentRecord>,
}

/// An attachment as stored: what clients see, plus where the bytes are.
#[derive(Debug, Clone)]
pub struct AttachmentRecord {
    pub id: String,
    pub note_id: String,
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub name: Option<String>,
    /// Hash of the file's bytes.
    pub sha256: String,
    pub file: BlobRef,
    pub preview: Option<BlobRef>,
}

impl AttachmentRecord {
    /// Changes whenever the file or its preview changes (see `Attachment::v`). Derived from the names, which include the
    /// owner and a hash of the content, so two users' files never share a version.
    pub fn version(&self) -> String {
        crate::blob::attachment_version(self.file.key(), self.preview.as_ref().map(BlobRef::key))
    }

    /// What clients see.
    pub fn public(&self) -> Attachment {
        Attachment {
            id: self.id.clone(),
            note_id: self.note_id.clone(),
            mime: self.mime.clone(),
            bytes: self.bytes,
            width: self.width,
            height: self.height,
            duration_ms: self.duration_ms,
            name: self.name.clone(),
            has_thumb: self.preview.is_some(),
            v: self.version(),
        }
    }
}

/// Which cards to return, already normalised by the service.
///
/// Matching: `terms` must each match the start of a word in the body (words are runs of letters and digits,
/// compared case-insensitively) and every term must match; `tag` is lowercase without `#`; `from` and `to` are inclusive
/// bounds on `created_at`.
///
/// Order: with `reminders_only`, cards with a pending reminder (a time set and not done), soonest first, then by id.
/// Otherwise display order: open-todo cards first, then by `position` (highest first), then newest id first.
#[derive(Debug, Clone, Default)]
pub struct RecordQuery {
    pub terms: Vec<String>,
    pub tag: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub reminders_only: bool,
    /// None means no limit.
    pub limit: Option<usize>,
    pub offset: usize,
}

/// Operations inside one unit of work.
pub trait RecordTx {
    /// Stores a new card under `n.id`. Tags in `n.tags` are stored with it; `attachments` is ignored.
    fn insert_note(&mut self, owner: &str, n: &NoteRecord) -> Result<()>;
    /// One card with its tags and attachments, or None.
    fn get_note(&mut self, owner: &str, id: &str) -> Result<Option<NoteRecord>>;
    /// Replaces body, `updated_at`, schedule, `open_todos` and tags. Position, creation time and attachments are kept.
    /// NotFound if the card is not the owner's.
    fn update_note(&mut self, owner: &str, n: &NoteRecord) -> Result<()>;
    /// Replaces only the tags. NotFound if the card is not the owner's.
    fn set_tags(&mut self, owner: &str, id: &str, tags: &[String]) -> Result<()>;
    /// Sets `updated_at` only.
    fn touch_note(&mut self, owner: &str, id: &str, updated_at: i64) -> Result<()>;
    /// Removes the card with its tags and attachments. False if it was not the owner's.
    fn delete_note(&mut self, owner: &str, id: &str) -> Result<bool>;
    /// Cards matching the query, with tags and attachments, in the order described on `RecordQuery`. Backends should
    /// load tags and attachments for the whole page at once.
    fn query_notes(&mut self, owner: &str, q: &RecordQuery) -> Result<Vec<NoteRecord>>;
    /// Position data for one card.
    fn slot(&mut self, owner: &str, id: &str) -> Result<Option<Slot>>;
    /// Position data for all of the owner's cards, in display order, with ids.
    fn slots(&mut self, owner: &str) -> Result<Vec<(String, Slot)>>;
    /// Sets one card's position. Nothing else changes: positions never need renumbering.
    fn set_position(&mut self, owner: &str, id: &str, position: &str) -> Result<()>;
    /// Tag names with how many cards use each: most used first, then by name.
    fn tag_counts(&mut self, owner: &str) -> Result<Vec<TagCount>>;

    /// In creation order.
    fn rules(&mut self, owner: &str) -> Result<Vec<Rule>>;
    fn insert_rule(&mut self, owner: &str, id: &str, pattern: &str, tag: &str, created_at: i64) -> Result<Rule>;
    /// False if it was not the owner's.
    fn delete_rule(&mut self, owner: &str, id: &str) -> Result<bool>;

    fn count_attachments(&mut self, owner: &str, note_id: &str) -> Result<i64>;
    /// Appends to the card's attachments (display order is insertion order). The card must be the owner's.
    fn insert_attachment(&mut self, owner: &str, id: &str, note_id: &str, a: &NewAttachment, created_at: i64) -> Result<AttachmentRecord>;
    fn get_attachment(&mut self, owner: &str, id: &str) -> Result<Option<AttachmentRecord>>;
    /// Sets the preview. NotFound if the attachment is not the owner's.
    fn set_thumb(&mut self, owner: &str, id: &str, preview: &BlobRef) -> Result<()>;
    fn delete_attachment(&mut self, owner: &str, id: &str) -> Result<Option<AttachmentRecord>>;
}

pub trait NoteRecords: Send + Sync + 'static {
    /// Runs `f` as one atomic unit.
    fn transact<T>(&self, f: impl FnOnce(&mut dyn RecordTx) -> Result<T>) -> Result<T>;
}
