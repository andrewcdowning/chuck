//! What the data plane's storage offers the control and management planes about an owner: how much they use, which
//! files they have, and erasing them. Counts and keys only; never content.

use crate::store::Result;

/// What an owner uses. Counts and sizes only; never content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub notes: i64,
    pub attachments: i64,
    pub storage_bytes: i64,
}

/// What the data plane's storage offers the control plane. Implemented by the backend that holds the owner's records.
pub trait OwnerRecords: Send + Sync + 'static {
    fn usage(&self, owner: &str) -> Result<Usage>;
    /// Storage keys of all of the owner's files and previews, without duplicates.
    fn blob_keys(&self, owner: &str) -> Result<Vec<String>>;
    /// Storage keys used by anyone's files and previews. What the orphan sweeper treats as in use.
    fn all_blob_keys(&self) -> Result<Vec<String>>;
    /// Removes everything the owner has from the data plane (cards, tags, attachments, rules, suggestion feedback) and
    /// forgets anything derived from it. Does nothing if there is nothing left. Does not touch the account record.
    fn purge_rows(&self, owner: &str) -> Result<()>;
}
