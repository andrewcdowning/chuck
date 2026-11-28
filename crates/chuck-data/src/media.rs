//! Files on cards: checking an upload, keeping its bytes, recording it on the card, and reading it back.
//!
//! This is the one place that connects the card records to the `BlobStore`. Callers hand over bytes and get back what clients
//! may see; where the bytes live (`BlobRef`) does not leave the data plane. Unused files are not deleted here: when an
//! attachment goes, its files are left for the sweeper, which removes whatever nothing refers to.

use crate::blob::{sha256_hex, BlobRef, BlobStore};
use crate::store::{Attachment, NewAttachment, NoteStore, StoreError};
use chuck_core::domain::input::clean_name;
use chuck_core::media::{self, MediaError};

/// Longest audio clip length a client may report, in milliseconds.
pub const MAX_DURATION_MS: i64 = 6 * 3600 * 1000;
/// Largest side of a preview image, in pixels.
pub const MAX_PREVIEW_SIDE: u32 = 1024;

#[derive(Debug)]
pub enum MediaFailure {
    /// The bytes are not an acceptable file (type, size or shape).
    Rejected(MediaError),
    /// The reported audio length is outside 0 to 6 hours.
    InvalidDuration,
    /// A preview larger than `MAX_PREVIEW_SIDE` on a side.
    PreviewTooLarge,
    /// The record exists but its file is not in the store.
    FileMissing,
    Store(StoreError),
}

impl From<StoreError> for MediaFailure {
    fn from(e: StoreError) -> Self {
        MediaFailure::Store(e)
    }
}

impl From<MediaError> for MediaFailure {
    fn from(e: MediaError) -> Self {
        MediaFailure::Rejected(e)
    }
}

/// A file read back for serving.
pub struct MediaFile {
    pub attachment: Attachment,
    pub bytes: Vec<u8>,
    /// These are the preview's bytes, not the original's.
    pub preview: bool,
}

pub struct MediaService<'a> {
    store: &'a dyn NoteStore,
    blobs: &'a dyn BlobStore,
}

fn storing(e: std::io::Error) -> MediaFailure {
    MediaFailure::Store(StoreError::Other(format!("storing file: {e}")))
}

impl<'a> MediaService<'a> {
    pub fn new(store: &'a dyn NoteStore, blobs: &'a dyn BlobStore) -> Self {
        Self { store, blobs }
    }

    /// Checks an uploaded file (its type comes from its bytes, never its name; metadata is stripped), keeps it, and attaches
    /// it to the card. `name` and `duration_ms` are display details from the client.
    pub fn add(&self, owner: &str, note_id: &str, raw: &[u8], name: Option<String>, duration_ms: Option<i64>) -> Result<Attachment, MediaFailure> {
        // Fail early, before any work, if the card is not this user's.
        self.store.get(owner, note_id)?;
        let c = media::check_upload(raw)?;
        let duration_ms = match duration_ms {
            Some(_) if !c.audio => None, // ignored for images
            Some(d) if !(0..=MAX_DURATION_MS).contains(&d) => return Err(MediaFailure::InvalidDuration),
            d => d,
        };
        let file = BlobRef::for_content(owner, &c.bytes);
        self.blobs.put(file.key(), &c.bytes).map_err(storing)?;
        let new = NewAttachment {
            mime: c.mime.to_string(),
            bytes: c.bytes.len() as i64,
            width: c.width.map(|w| w as i64),
            height: c.height.map(|h| h as i64),
            duration_ms,
            name: clean_name(name),
            sha256: sha256_hex(&c.bytes),
            file,
        };
        // If recording fails, the stored file is left for the sweeper to remove if nothing else uses it.
        Ok(self.store.add_attachment(owner, note_id, &new)?)
    }

    /// Replaces the small preview image of an attachment.
    pub fn set_preview(&self, owner: &str, id: &str, raw: &[u8]) -> Result<(), MediaFailure> {
        self.store.attachment(owner, id)?;
        let c = media::check_image(raw, media::MAX_THUMB_BYTES)?;
        if c.width > MAX_PREVIEW_SIDE || c.height > MAX_PREVIEW_SIDE {
            return Err(MediaFailure::PreviewTooLarge);
        }
        let preview = BlobRef::for_content(owner, &c.bytes);
        self.blobs.put(preview.key(), &c.bytes).map_err(storing)?;
        Ok(self.store.set_attachment_thumb(owner, id, &preview)?) // the replaced preview is left to the sweeper
    }

    /// The bytes of an attachment, or of its preview when `preview` is asked for and there is one.
    pub fn read(&self, owner: &str, id: &str, preview: bool) -> Result<MediaFile, MediaFailure> {
        let attachment = self.store.attachment(owner, id)?;
        let files = self.store.attachment_files(owner, id)?;
        let (r, is_preview) = match (files.preview, preview) {
            (Some(p), true) => (p, true),
            _ => (files.file, false),
        };
        let bytes = self.blobs.get(r.key()).map_err(|_| MediaFailure::FileMissing)?;
        Ok(MediaFile { attachment, bytes, preview: is_preview })
    }

    /// Removes an attachment. Its files go when nothing refers to them any more (see the sweeper).
    pub fn delete(&self, owner: &str, id: &str) -> Result<(), StoreError> {
        self.store.delete_attachment(owner, id).map(|_| ())
    }
}
