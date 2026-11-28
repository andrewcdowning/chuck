//! The data-plane behaviour for cards, tags, rules and attachments, written once over `NoteRecords`.
//!
//! Everything a user can observe about these operations is decided here (or in `domain`): what an update means, which
//! errors are the client's fault, how cards are ordered and placed, what a search matches. Backends only store records.

use chuck_core::domain::card::{card_kind, open_items, tags_for};
use chuck_core::domain::input;
use chuck_core::domain::order;
use chuck_core::domain::schedule::{self, ScheduleInput};
use chuck_core::domain::search;
use crate::records::{NoteRecord, NoteRecords, RecordQuery, RecordTx};
use crate::store::*;
use std::time::{SystemTime, UNIX_EPOCH};

/// Most auto-tag rules one user may have.
pub const MAX_RULES: usize = 200;
/// Most cards one page may hold, and the default page size.
pub const MAX_PAGE: u32 = 200;
pub const DEFAULT_PAGE: u32 = 50;

pub fn system_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub struct NoteService<'a, R: NoteRecords> {
    records: &'a R,
    now: fn() -> i64,
}

fn schedule_input(i: &NoteInput) -> ScheduleInput<'_> {
    ScheduleInput { remind_at: i.remind_at, repeat: i.repeat.as_deref(), reminder_done: i.reminder_done, due_at: i.due_at }
}

fn move_error(e: order::MoveError) -> StoreError {
    StoreError::Invalid(e.message().into())
}

fn to_note(r: NoteRecord) -> Note {
    Note {
        id: r.id,
        kind: card_kind(&r.body, r.schedule.remind_at, !r.attachments.is_empty()),
        body: r.body,
        tags: r.tags,
        created_at: r.created_at,
        updated_at: r.updated_at,
        remind_at: r.schedule.remind_at,
        repeat: r.schedule.repeat,
        reminder_done: r.schedule.reminder_done,
        due_at: r.schedule.due_at,
        attachments: r.attachments,
    }
}

fn rule_pairs(tx: &mut dyn RecordTx, owner: &str) -> Result<Vec<(String, String)>> {
    Ok(tx.rules(owner)?.into_iter().map(|r| (r.pattern, r.tag)).collect())
}

fn new_position(tx: &mut dyn RecordTx, owner: &str, open_todo: bool, due: Option<i64>) -> Result<String> {
    let slots: Vec<_> = tx.slots(owner)?.into_iter().map(|(_, s)| s).collect();
    Ok(order::place_new(&slots, open_todo, due))
}

impl<'a, R: NoteRecords> NoteService<'a, R> {
    pub fn new(records: &'a R) -> Self {
        Self { records, now: system_now }
    }

    /// For tests: a service with its own clock.
    pub fn with_clock(records: &'a R, now: fn() -> i64) -> Self {
        Self { records, now }
    }

    pub fn create(&self, owner: &str, input: &NoteInput) -> Result<Note> {
        let t = (self.now)();
        self.records.transact(|tx| {
            let body = input.body.as_str();
            let sch = schedule::for_new(&schedule_input(input));
            let open = open_items(body);
            let position = new_position(tx, owner, open > 0, sch.due_at)?;
            let tags = tags_for(&rule_pairs(tx, owner)?, body);
            let rec = NoteRecord { id: 0, body: body.to_string(), created_at: t, updated_at: t, schedule: sch, open_todos: open, position, tags, attachments: vec![] };
            let id = tx.insert_note(owner, &rec)?;
            tx.get_note(owner, id)?.map(to_note).ok_or(StoreError::NotFound)
        })
    }

    pub fn update(&self, owner: &str, id: i64, input: &NoteInput) -> Result<Note> {
        let t = (self.now)();
        self.records.transact(|tx| {
            let cur = tx.get_note(owner, id)?.ok_or(StoreError::NotFound)?;
            let body = input.body.as_str();
            let rec = NoteRecord {
                body: body.to_string(),
                updated_at: t,
                schedule: schedule::merged(&cur.schedule, &schedule_input(input)),
                open_todos: open_items(body),
                tags: tags_for(&rule_pairs(tx, owner)?, body),
                ..cur
            };
            tx.update_note(owner, &rec)?;
            tx.get_note(owner, id)?.map(to_note).ok_or(StoreError::NotFound)
        })
    }

    pub fn delete(&self, owner: &str, id: i64) -> Result<()> {
        self.records.transact(|tx| if tx.delete_note(owner, id)? { Ok(()) } else { Err(StoreError::NotFound) })
    }

    pub fn list(&self, owner: &str, q: &NoteQuery) -> Result<Vec<Note>> {
        let rq = RecordQuery {
            terms: q.q.as_deref().map(search::terms).unwrap_or_default(),
            tag: q.tag.as_deref().filter(|t| !t.is_empty()).map(|t| t.trim_start_matches('#').to_lowercase()),
            from: q.from,
            to: q.to,
            reminders_only: q.reminders.unwrap_or(false),
            limit: Some(q.limit.unwrap_or(DEFAULT_PAGE).min(MAX_PAGE) as usize),
            offset: q.offset.unwrap_or(0) as usize,
        };
        self.records.transact(|tx| Ok(tx.query_notes(owner, &rq)?.into_iter().map(to_note).collect()))
    }

    pub fn get(&self, owner: &str, id: i64) -> Result<Note> {
        self.records.transact(|tx| tx.get_note(owner, id)?.map(to_note).ok_or(StoreError::NotFound))
    }

    pub fn tags(&self, owner: &str) -> Result<Vec<TagCount>> {
        self.records.transact(|tx| tx.tag_counts(owner))
    }

    pub fn move_note(&self, owner: &str, id: i64, prev: Option<i64>, next: Option<i64>) -> Result<Note> {
        self.records.transact(|tx| {
            // Ownership first: someone else's card is NotFound whatever else is wrong with the request.
            let me = tx.slot(owner, id)?.ok_or(StoreError::NotFound)?;
            order::check_request(id, prev, next).map_err(move_error)?;
            let neighbour = |tx: &mut dyn RecordTx, n: Option<i64>| -> Result<Option<(String, bool)>> {
                n.map(|n| tx.slot(owner, n)?.map(|s| (s.pos, s.pinned)).ok_or(StoreError::NotFound)).transpose()
            };
            let above = neighbour(tx, prev)?;
            let below = neighbour(tx, next)?;
            order::check_neighbours(me.pinned, above.as_ref().map(|(p, g)| (p.as_str(), *g)), below.as_ref().map(|(p, g)| (p.as_str(), *g))).map_err(move_error)?;
            let pos = order::key_between(below.as_ref().map(|x| x.0.as_str()), above.as_ref().map(|x| x.0.as_str()));
            tx.set_position(owner, id, &pos)?;
            tx.get_note(owner, id)?.map(to_note).ok_or(StoreError::NotFound)
        })
    }

    pub fn rules(&self, owner: &str) -> Result<Vec<Rule>> {
        self.records.transact(|tx| tx.rules(owner))
    }

    pub fn add_rule(&self, owner: &str, pattern: &str, tag: &str) -> Result<Rule> {
        let t = (self.now)();
        self.records.transact(|tx| {
            if tx.rules(owner)?.len() >= MAX_RULES {
                return Err(StoreError::Invalid("too many rules".into()));
            }
            tx.insert_rule(owner, pattern, tag, t)
        })
    }

    pub fn delete_rule(&self, owner: &str, id: i64) -> Result<()> {
        self.records.transact(|tx| if tx.delete_rule(owner, id)? { Ok(()) } else { Err(StoreError::NotFound) })
    }

    /// Re-tags every card with the current rules. Returns how many cards changed.
    pub fn apply_rules(&self, owner: &str) -> Result<usize> {
        self.records.transact(|tx| {
            let rules = rule_pairs(tx, owner)?;
            let all = tx.query_notes(owner, &RecordQuery::default())?;
            let mut changed = 0;
            for n in all {
                let mut after = tags_for(&rules, &n.body);
                after.sort();
                let mut before = n.tags.clone();
                before.sort();
                if before != after {
                    tx.set_tags(owner, n.id, &after)?;
                    changed += 1;
                }
            }
            Ok(changed)
        })
    }

    pub fn add_attachment(&self, owner: &str, note_id: i64, a: &NewAttachment) -> Result<Attachment> {
        let t = (self.now)();
        self.records.transact(|tx| {
            tx.slot(owner, note_id)?.ok_or(StoreError::NotFound)?;
            if !input::has_room_for_attachment(tx.count_attachments(owner, note_id)?) {
                return Err(StoreError::Invalid("too many attachments on this card".into()));
            }
            let att = tx.insert_attachment(owner, note_id, a, t)?;
            tx.touch_note(owner, note_id, t)?;
            Ok(att)
        })
    }

    pub fn attachment(&self, owner: &str, id: i64) -> Result<Attachment> {
        self.records.transact(|tx| tx.get_attachment(owner, id)?.ok_or(StoreError::NotFound))
    }

    /// Returns the key of the preview this replaced, if it was a different one.
    pub fn set_attachment_thumb(&self, owner: &str, id: i64, thumb_key: &str) -> Result<Option<String>> {
        self.records.transact(|tx| Ok(tx.set_thumb(owner, id, thumb_key)?.filter(|k| k != thumb_key)))
    }

    pub fn delete_attachment(&self, owner: &str, id: i64) -> Result<Attachment> {
        self.records.transact(|tx| tx.delete_attachment(owner, id)?.ok_or(StoreError::NotFound))
    }

    pub fn blob_referenced(&self, owner: &str, key: &str) -> Result<bool> {
        self.records.transact(|tx| tx.blob_referenced(owner, key))
    }
}

/// A store that keeps its cards in a `NoteRecords` backend (possibly itself).
pub trait HasRecords {
    type R: NoteRecords;
    fn records(&self) -> &Self::R;
}

/// Implements the notes, tags, rules and attachments methods of `NoteStore` by handing them to a `NoteService` over
/// `HasRecords::records(self)`. Use it inside `impl NoteStore for X { ... }`.
#[macro_export]
macro_rules! note_service_methods {
    () => {
        fn create(&self, owner: &str, input: &$crate::store::NoteInput) -> $crate::store::Result<$crate::store::Note> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).create(owner, input)
        }
        fn update(&self, owner: &str, id: i64, input: &$crate::store::NoteInput) -> $crate::store::Result<$crate::store::Note> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).update(owner, id, input)
        }
        fn delete(&self, owner: &str, id: i64) -> $crate::store::Result<()> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).delete(owner, id)
        }
        fn list(&self, owner: &str, q: &$crate::store::NoteQuery) -> $crate::store::Result<Vec<$crate::store::Note>> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).list(owner, q)
        }
        fn tags(&self, owner: &str) -> $crate::store::Result<Vec<$crate::store::TagCount>> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).tags(owner)
        }
        fn get(&self, owner: &str, id: i64) -> $crate::store::Result<$crate::store::Note> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).get(owner, id)
        }
        fn move_note(&self, owner: &str, id: i64, prev: Option<i64>, next: Option<i64>) -> $crate::store::Result<$crate::store::Note> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).move_note(owner, id, prev, next)
        }
        fn rules(&self, owner: &str) -> $crate::store::Result<Vec<$crate::store::Rule>> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).rules(owner)
        }
        fn add_rule(&self, owner: &str, pattern: &str, tag: &str) -> $crate::store::Result<$crate::store::Rule> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).add_rule(owner, pattern, tag)
        }
        fn delete_rule(&self, owner: &str, id: i64) -> $crate::store::Result<()> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).delete_rule(owner, id)
        }
        fn apply_rules(&self, owner: &str) -> $crate::store::Result<usize> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).apply_rules(owner)
        }
        fn add_attachment(&self, owner: &str, note_id: i64, a: &$crate::store::NewAttachment) -> $crate::store::Result<$crate::store::Attachment> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).add_attachment(owner, note_id, a)
        }
        fn attachment(&self, owner: &str, id: i64) -> $crate::store::Result<$crate::store::Attachment> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).attachment(owner, id)
        }
        fn set_attachment_thumb(&self, owner: &str, id: i64, thumb_key: &str) -> $crate::store::Result<Option<String>> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).set_attachment_thumb(owner, id, thumb_key)
        }
        fn delete_attachment(&self, owner: &str, id: i64) -> $crate::store::Result<$crate::store::Attachment> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).delete_attachment(owner, id)
        }
        fn blob_referenced(&self, owner: &str, key: &str) -> $crate::store::Result<bool> {
            $crate::service::NoteService::new($crate::service::HasRecords::records(self)).blob_referenced(owner, key)
        }
    };
}
