//! Tag suggestions as a service over a thin `TagSource`.
//!
//! The learning itself is pure (`tagging::Model`). This module decides when a model is rebuilt and what feeds it, so that
//! no storage backend has to know any of it. A backend only answers a few plain questions: the text of an owner's cards,
//! their rules, their feedback, and a version number that changes when their card text changes.

use chuck_core::domain::card::{extract_tags, tags_for};
use crate::store::{FeedbackEvent, Result};
use chuck_core::tagging::{feedback_offset, strip_hashtags, Doc, Model, SuggestionSet, MIN_DOCS_TO_CALIBRATE};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many users' trained models are kept. The least recently used one is dropped when a new one needs room.
pub const MODEL_CACHE_SIZE: usize = 256;

/// What a storage backend provides for tag suggestions. Every method is scoped to one owner and must not reveal anything
/// about other owners.
pub trait TagSource: Send + Sync + 'static {
    /// A number that is different after any change to the text of this owner's cards (a card added, edited or removed)
    /// and unchanged by other owners' changes. It may restart at any value when the process restarts.
    fn training_version(&self, owner: &str) -> Result<u64>;
    /// The text of all of the owner's cards.
    fn bodies(&self, owner: &str) -> Result<Vec<String>>;
    /// The owner's auto-tag rules as `(pattern, tag)`, in creation order.
    fn rule_pairs(&self, owner: &str) -> Result<Vec<(String, String)>>;
    /// Accepted and dismissed counts per tag.
    fn feedback_counts(&self, owner: &str) -> Result<Vec<FeedbackCount>>;
    fn record_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64, at: i64) -> Result<()>;
    /// All feedback, oldest first (part of the owner's data export).
    fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>>;
    /// Owners that have cards, with how many each. Used by the command-line tools.
    fn owners(&self) -> Result<Vec<(String, i64)>>;
}

#[derive(Debug, Clone)]
pub struct FeedbackCount {
    pub tag: String,
    pub accepted: u32,
    pub dismissed: u32,
}

/// Notes that carry at least one tag the user wrote, with the hashtags stripped from the text. Tags added by auto-tag
/// rules are deliberately not labels.
pub fn training_docs(source: &dyn TagSource, owner: &str) -> Result<Vec<Doc>> {
    Ok(source
        .bodies(owner)?
        .iter()
        .map(|b| Doc { labels: extract_tags(b), text: strip_hashtags(b) })
        .filter(|d| !d.labels.is_empty())
        .collect())
}

struct Entry {
    version: u64,
    model: Arc<Model>,
    last_used: u64,
}

/// Trained models by owner, each valid for one `training_version`. Bounded, so memory does not grow with the number of
/// users.
pub struct ModelCache {
    entries: Mutex<HashMap<String, Entry>>,
    capacity: usize,
    tick: AtomicU64,
    trained: AtomicU64,
}

impl Default for ModelCache {
    fn default() -> Self {
        Self::new(MODEL_CACHE_SIZE)
    }
}

impl ModelCache {
    pub fn new(capacity: usize) -> Self {
        Self { entries: Mutex::new(HashMap::new()), capacity: capacity.max(1), tick: AtomicU64::new(0), trained: AtomicU64::new(0) }
    }

    /// The cached model if it is for this version, else the result of `train`, which is then cached.
    fn get_or_train(&self, owner: &str, version: u64, train: impl FnOnce() -> Result<Model>) -> Result<Arc<Model>> {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = self.entries.lock().unwrap().get_mut(owner) {
            if e.version == version {
                e.last_used = tick;
                return Ok(e.model.clone());
            }
        }
        let model = Arc::new(train()?);
        self.trained.fetch_add(1, Ordering::Relaxed);
        let mut entries = self.entries.lock().unwrap();
        if !entries.contains_key(owner) && entries.len() >= self.capacity {
            if let Some(oldest) = entries.iter().min_by_key(|(_, e)| e.last_used).map(|(k, _)| k.clone()) {
                entries.remove(&oldest);
            }
        }
        entries.insert(owner.to_string(), Entry { version, model: model.clone(), last_used: tick });
        Ok(model)
    }

    /// Drops the owner's model (their account was deleted).
    pub fn forget(&self, owner: &str) {
        self.entries.lock().unwrap().remove(owner);
    }

    /// How many models have been trained since the cache was made. For tests and diagnostics.
    pub fn trained(&self) -> u64 {
        self.trained.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct TagModelService<'a> {
    source: &'a dyn TagSource,
    cache: &'a ModelCache,
    now: fn() -> i64,
}

impl<'a> TagModelService<'a> {
    pub fn new(source: &'a dyn TagSource, cache: &'a ModelCache) -> Self {
        Self { source, cache, now: crate::service::system_now }
    }

    /// Tags the owner already uses that fit `text`, best first, with calibrated confidence.
    pub fn suggest(&self, owner: &str, text: &str) -> Result<SuggestionSet> {
        // Read the version first: if a card changes while we train, the model is stamped with the older version and is
        // rebuilt on the next call, never the other way round.
        let version = self.source.training_version(owner)?;
        let model = self.cache.get_or_train(owner, version, || Ok(Model::train(&training_docs(self.source, owner)?)))?;
        // Tags already written, or about to be added by a rule, are not suggestions.
        let taken = tags_for(&self.source.rule_pairs(owner)?, text);
        let offsets: HashMap<String, f64> =
            self.source.feedback_counts(owner)?.into_iter().map(|f| (f.tag, feedback_offset(f.accepted, f.dismissed))).collect();
        Ok(SuggestionSet {
            suggestions: model.suggest(text, &taken, &offsets),
            tagged_notes: model.docs,
            calibrated: model.calibrated,
            calibrate_at: MIN_DOCS_TO_CALIBRATE,
        })
    }

    /// Logs that a suggestion was accepted or dismissed; this adjusts that tag's future confidence.
    pub fn record_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64) -> Result<()> {
        self.source.record_feedback(owner, tag, accepted, confidence.clamp(0.0, 1.0), (self.now)())
    }

    pub fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>> {
        self.source.feedback_events(owner)
    }
}

/// A store that can build a `TagModelService` over itself.
pub trait HasTagModel {
    fn tag_model(&self) -> TagModelService<'_>;
}

/// Implements the suggestion methods of `NoteStore` through `HasTagModel`. Use inside `impl NoteStore for X { ... }`.
#[macro_export]
macro_rules! tag_service_methods {
    () => {
        fn suggest_tags(&self, owner: &str, text: &str) -> $crate::store::Result<$crate::tagging::SuggestionSet> {
            $crate::tagmodel::HasTagModel::tag_model(self).suggest(owner, text)
        }
        fn record_suggestion_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64) -> $crate::store::Result<()> {
            $crate::tagmodel::HasTagModel::tag_model(self).record_feedback(owner, tag, accepted, confidence)
        }
        fn feedback_events(&self, owner: &str) -> $crate::store::Result<Vec<$crate::store::FeedbackEvent>> {
            $crate::tagmodel::HasTagModel::tag_model(self).feedback_events(owner)
        }
    };
}
