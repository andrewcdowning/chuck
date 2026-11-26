//! Storage abstraction. The HTTP layer only talks to `NoteStore`, so a hosted
//! deployment can add a Postgres/DynamoDB implementation without touching `api.rs`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
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
}
