//! A reference `NoteRecords` backend that keeps everything in memory.
//!
//! Its purpose is to prove that the record traits are thin enough for a very different backend: it has no SQL, no search
//! index and no stored order, yet `conformance::run_notes` passes on it, so every behaviour users see comes from
//! `NoteService`. It is also a template for a real backend (a key-value or document store). Not for production: data is
//! lost on exit, and each unit of work copies the whole state to stay atomic.
//!
//! `MemStore` implements all of `NoteStore`, accounts included, so every conformance case runs on it.

use chuck_control::{AccountRecords, OwnerRecords, Usage};
use chuck_core::domain::order::Slot;
use chuck_data::records::{NoteRecord, NoteRecords, RecordQuery, RecordTx};
use chuck_data::service::HasRecords;
use chuck_data::store::*;
use chuck_data::tagmodel::{FeedbackCount, HasTagModel, ModelCache, TagModelService, TagSource};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

#[derive(Clone, Default)]
struct State {
    notes: BTreeMap<String, (String, NoteRecord)>,
    /// (owner, position within the card, attachment); the card is `attachment.note_id`
    attachments: BTreeMap<String, (String, i64, Attachment)>,
    rules: BTreeMap<String, (String, Rule)>,
    versions: HashMap<String, u64>,
    feedback: Vec<(String, FeedbackEvent)>,
    users: BTreeMap<String, User>,
}

#[derive(Default)]
pub struct MemRecords {
    state: Mutex<State>,
}

impl MemRecords {
    pub fn new() -> Self {
        Self::default()
    }
}

struct MemTx<'a> {
    s: &'a mut State,
    changed: HashSet<String>,
}

fn words(body: &str) -> Vec<String> {
    body.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

fn display_order(a: &(String, &NoteRecord), b: &(String, &NoteRecord)) -> std::cmp::Ordering {
    let (pa, pb) = (a.1.open_todos > 0, b.1.open_todos > 0);
    pb.cmp(&pa)
        .then(b.1.position.cmp(&a.1.position))
        .then(b.0.cmp(&a.0))
}

impl MemTx<'_> {
    fn full(&self, id: &str) -> Option<NoteRecord> {
        let (_, rec) = self.s.notes.get(id)?;
        let mut rec = rec.clone();
        let mut atts: Vec<(i64, String, Attachment)> =
            self.s.attachments.iter().filter(|(_, (_, _, a))| a.note_id == id).map(|(aid, (_, pos, a))| (*pos, aid.clone(), a.clone())).collect();
        atts.sort_by_key(|(pos, aid, _)| (*pos, aid.clone()));
        rec.attachments = atts.into_iter().map(|(_, _, a)| a).collect();
        Some(rec)
    }

    fn mine(&self, owner: &str, id: &str) -> bool {
        self.s.notes.get(id).is_some_and(|(o, _)| o == owner)
    }
}

impl RecordTx for MemTx<'_> {
    fn insert_note(&mut self, owner: &str, n: &NoteRecord) -> Result<()> {
        let id = n.id.clone();
        let mut rec = n.clone();
        rec.attachments = vec![];
        rec.tags.sort();
        self.s.notes.insert(id, (owner.to_string(), rec));
        self.changed.insert(owner.to_string());
        Ok(())
    }

    fn get_note(&mut self, owner: &str, id: &str) -> Result<Option<NoteRecord>> {
        Ok(if self.mine(owner, id) { self.full(id) } else { None })
    }

    fn update_note(&mut self, owner: &str, n: &NoteRecord) -> Result<()> {
        if !self.mine(owner, &n.id) {
            return Err(StoreError::NotFound);
        }
        let (_, rec) = self.s.notes.get_mut(&n.id).unwrap();
        rec.body = n.body.clone();
        rec.updated_at = n.updated_at;
        rec.schedule = n.schedule.clone();
        rec.open_todos = n.open_todos;
        rec.tags = n.tags.clone();
        rec.tags.sort();
        self.changed.insert(owner.to_string());
        Ok(())
    }

    fn set_tags(&mut self, owner: &str, id: &str, tags: &[String]) -> Result<()> {
        if !self.mine(owner, id) {
            return Err(StoreError::NotFound);
        }
        let (_, rec) = self.s.notes.get_mut(id).unwrap();
        rec.tags = tags.to_vec();
        rec.tags.sort();
        rec.tags.dedup();
        Ok(())
    }

    fn touch_note(&mut self, owner: &str, id: &str, updated_at: i64) -> Result<()> {
        if self.mine(owner, id) {
            self.s.notes.get_mut(id).unwrap().1.updated_at = updated_at;
        }
        Ok(())
    }

    fn delete_note(&mut self, owner: &str, id: &str) -> Result<bool> {
        if !self.mine(owner, id) {
            return Ok(false);
        }
        self.s.notes.remove(id);
        self.s.attachments.retain(|_, (_, _, a)| a.note_id != id);
        self.changed.insert(owner.to_string());
        Ok(true)
    }

    fn query_notes(&mut self, owner: &str, q: &RecordQuery) -> Result<Vec<NoteRecord>> {
        let mut hits: Vec<(String, &NoteRecord)> = self
            .s
            .notes
            .iter()
            .filter(|(_, (o, _))| o == owner)
            .map(|(id, (_, r))| (id.clone(), r))
            .filter(|(_, r)| {
                let w = if q.terms.is_empty() { vec![] } else { words(&r.body) };
                q.terms.iter().all(|t| {
                    let t = t.to_lowercase();
                    w.iter().any(|w| w.starts_with(&t))
                }) && q.tag.as_ref().map_or(true, |t| r.tags.contains(t))
                    && q.from.map_or(true, |f| r.created_at >= f)
                    && q.to.map_or(true, |t| r.created_at <= t)
                    && (!q.reminders_only || (r.schedule.remind_at.is_some() && !r.schedule.reminder_done))
            })
            .collect();
        if q.reminders_only {
            hits.sort_by_key(|(id, r)| (r.schedule.remind_at, id.clone()));
        } else {
            hits.sort_by(display_order);
        }
        let ids: Vec<String> = hits.into_iter().map(|(id, _)| id).skip(q.offset).take(q.limit.unwrap_or(usize::MAX)).collect();
        Ok(ids.into_iter().filter_map(|id| self.full(&id)).collect())
    }

    fn slot(&mut self, owner: &str, id: &str) -> Result<Option<Slot>> {
        Ok(self.s.notes.get(id).filter(|(o, _)| o == owner).map(|(_, r)| Slot { pos: r.position.clone(), pinned: r.open_todos > 0, due: r.schedule.due_at }))
    }

    fn slots(&mut self, owner: &str) -> Result<Vec<(String, Slot)>> {
        let mut all: Vec<(String, &NoteRecord)> = self.s.notes.iter().filter(|(_, (o, _))| o == owner).map(|(id, (_, r))| (id.clone(), r)).collect();
        all.sort_by(display_order);
        Ok(all.into_iter().map(|(id, r)| (id, Slot { pos: r.position.clone(), pinned: r.open_todos > 0, due: r.schedule.due_at })).collect())
    }

    fn set_position(&mut self, owner: &str, id: &str, position: &str) -> Result<()> {
        if self.mine(owner, id) {
            self.s.notes.get_mut(id).unwrap().1.position = position.to_string();
        }
        Ok(())
    }

    fn tag_counts(&mut self, owner: &str) -> Result<Vec<TagCount>> {
        let mut counts: BTreeMap<String, i64> = BTreeMap::new();
        for (_, (o, r)) in &self.s.notes {
            if o == owner {
                for t in &r.tags {
                    *counts.entry(t.clone()).or_default() += 1;
                }
            }
        }
        let mut v: Vec<TagCount> = counts.into_iter().map(|(tag, count)| TagCount { tag, count }).collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then(a.tag.cmp(&b.tag)));
        Ok(v)
    }

    fn rules(&mut self, owner: &str) -> Result<Vec<Rule>> {
        Ok(self.s.rules.values().filter(|(o, _)| o == owner).map(|(_, r)| r.clone()).collect())
    }

    fn insert_rule(&mut self, owner: &str, id: &str, pattern: &str, tag: &str, _created_at: i64) -> Result<Rule> {
        let r = Rule { id: id.to_string(), pattern: pattern.to_string(), tag: tag.to_string() };
        self.s.rules.insert(r.id.clone(), (owner.to_string(), r.clone()));
        Ok(r)
    }

    fn delete_rule(&mut self, owner: &str, id: &str) -> Result<bool> {
        if self.s.rules.get(id).is_some_and(|(o, _)| o == owner) {
            self.s.rules.remove(id);
            return Ok(true);
        }
        Ok(false)
    }

    fn count_attachments(&mut self, owner: &str, note_id: &str) -> Result<i64> {
        Ok(self.s.attachments.values().filter(|(o, _, a)| o == owner && a.note_id == note_id).count() as i64)
    }

    fn insert_attachment(&mut self, owner: &str, id: &str, note_id: &str, a: &NewAttachment, _created_at: i64) -> Result<Attachment> {
        if !self.mine(owner, note_id) {
            return Err(StoreError::NotFound);
        }
        let pos = self.s.attachments.values().filter(|(_, _, a)| a.note_id == note_id).map(|(_, pos, _)| *pos + 1).max().unwrap_or(0);
        let att = Attachment {
            id: id.to_string(),
            note_id: note_id.to_string(),
            mime: a.mime.clone(),
            bytes: a.bytes,
            width: a.width,
            height: a.height,
            duration_ms: a.duration_ms,
            name: a.name.clone(),
            has_thumb: false,
            v: chuck_data::blob::attachment_version(&a.blob_key, None),
            blob_key: a.blob_key.clone(),
            thumb_key: None,
        };
        self.s.attachments.insert(id.to_string(), (owner.to_string(), pos, att.clone()));
        Ok(att)
    }

    fn get_attachment(&mut self, owner: &str, id: &str) -> Result<Option<Attachment>> {
        Ok(self.s.attachments.get(id).filter(|(o, _, _)| o == owner).map(|(_, _, a)| a.clone()))
    }

    fn set_thumb(&mut self, owner: &str, id: &str, thumb_key: &str) -> Result<Option<String>> {
        match self.s.attachments.get_mut(id) {
            Some((o, _, a)) if o == owner => {
                let old = a.thumb_key.replace(thumb_key.to_string());
                a.has_thumb = true;
                a.v = chuck_data::blob::attachment_version(&a.blob_key, a.thumb_key.as_deref());
                Ok(old)
            }
            _ => Err(StoreError::NotFound),
        }
    }

    fn delete_attachment(&mut self, owner: &str, id: &str) -> Result<Option<Attachment>> {
        if self.s.attachments.get(id).is_some_and(|(o, _, _)| o == owner) {
            return Ok(self.s.attachments.remove(id).map(|(_, _, a)| a));
        }
        Ok(None)
    }

    fn blob_referenced(&mut self, owner: &str, key: &str) -> Result<bool> {
        Ok(self.s.attachments.values().any(|(o, _, a)| o == owner && (a.blob_key == key || a.thumb_key.as_deref() == Some(key))))
    }
}

impl NoteRecords for MemRecords {
    fn transact<T>(&self, f: impl FnOnce(&mut dyn RecordTx) -> Result<T>) -> Result<T> {
        let mut guard = self.state.lock().unwrap();
        let mut work = guard.clone();
        let mut tx = MemTx { s: &mut work, changed: HashSet::new() };
        let out = f(&mut tx);
        let changed = std::mem::take(&mut tx.changed);
        if out.is_ok() {
            for owner in changed {
                *work.versions.entry(owner).or_insert(0) += 1;
            }
            *guard = work;
        }
        out
    }
}

impl TagSource for MemRecords {
    fn training_version(&self, owner: &str) -> Result<u64> {
        Ok(self.state.lock().unwrap().versions.get(owner).copied().unwrap_or(0))
    }

    fn bodies(&self, owner: &str) -> Result<Vec<String>> {
        Ok(self.state.lock().unwrap().notes.values().filter(|(o, _)| o == owner).map(|(_, r)| r.body.clone()).collect())
    }

    fn rule_pairs(&self, owner: &str) -> Result<Vec<(String, String)>> {
        Ok(self.state.lock().unwrap().rules.values().filter(|(o, _)| o == owner).map(|(_, r)| (r.pattern.clone(), r.tag.clone())).collect())
    }

    fn feedback_counts(&self, owner: &str) -> Result<Vec<FeedbackCount>> {
        let mut by_tag: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for (o, e) in &self.state.lock().unwrap().feedback {
            if o == owner {
                let c = by_tag.entry(e.tag.clone()).or_default();
                if e.action == "accepted" {
                    c.0 += 1;
                } else {
                    c.1 += 1;
                }
            }
        }
        Ok(by_tag.into_iter().map(|(tag, (accepted, dismissed))| FeedbackCount { tag, accepted, dismissed }).collect())
    }

    fn record_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64, at: i64) -> Result<()> {
        let action = if accepted { "accepted" } else { "dismissed" }.to_string();
        self.state.lock().unwrap().feedback.push((owner.to_string(), FeedbackEvent { tag: tag.to_string(), action, confidence, created_at: at }));
        Ok(())
    }

    fn feedback_events(&self, owner: &str) -> Result<Vec<FeedbackEvent>> {
        Ok(self.state.lock().unwrap().feedback.iter().filter(|(o, _)| o == owner).map(|(_, e)| e.clone()).collect())
    }

    fn owners(&self) -> Result<Vec<(String, i64)>> {
        let mut counts: BTreeMap<String, i64> = BTreeMap::new();
        for (o, _) in self.state.lock().unwrap().notes.values() {
            *counts.entry(o.clone()).or_default() += 1;
        }
        Ok(counts.into_iter().collect())
    }
}

/// The notes part of `NoteStore` over `MemRecords`.
#[derive(Default)]
pub struct MemStore {
    records: MemRecords,
    models: ModelCache,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AccountRecords for MemRecords {
    fn account(&self, id: &str) -> Result<Option<User>> {
        Ok(self.state.lock().unwrap().users.get(id).cloned())
    }

    fn insert_account_if_absent(&self, u: &User) -> Result<bool> {
        let mut st = self.state.lock().unwrap();
        if st.users.contains_key(&u.id) {
            return Ok(false);
        }
        st.users.insert(u.id.clone(), u.clone());
        Ok(true)
    }

    fn update_account_profile(&self, id: &str, display_name: Option<&str>, role: &str, last_seen_at: Option<i64>) -> Result<()> {
        if let Some(u) = self.state.lock().unwrap().users.get_mut(id) {
            u.display_name = display_name.map(String::from);
            u.role = role.to_string();
            u.last_seen_at = last_seen_at;
        }
        Ok(())
    }

    fn set_account_status(&self, id: &str, status: &str) -> Result<bool> {
        match self.state.lock().unwrap().users.get_mut(id) {
            Some(u) => {
                u.status = status.to_string();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn list_accounts(&self) -> Result<Vec<User>> {
        let mut v: Vec<User> = self.state.lock().unwrap().users.values().cloned().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(v)
    }

    fn remove_account(&self, id: &str) -> Result<bool> {
        Ok(self.state.lock().unwrap().users.remove(id).is_some())
    }
}

impl OwnerRecords for MemRecords {
    fn usage(&self, owner: &str) -> Result<Usage> {
        let st = self.state.lock().unwrap();
        let notes = st.notes.values().filter(|(o, _)| o == owner).count() as i64;
        let (attachments, storage_bytes) =
            st.attachments.values().filter(|(o, _, _)| o == owner).fold((0, 0), |(n, b), (_, _, a)| (n + 1, b + a.bytes));
        Ok(Usage { notes, attachments, storage_bytes })
    }

    fn blob_keys(&self, owner: &str) -> Result<Vec<String>> {
        let st = self.state.lock().unwrap();
        let mut keys: Vec<String> =
            st.attachments.values().filter(|(o, _, _)| o == owner).flat_map(|(_, _, a)| std::iter::once(a.blob_key.clone()).chain(a.thumb_key.clone())).collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    fn all_blob_keys(&self) -> Result<Vec<String>> {
        let st = self.state.lock().unwrap();
        let mut keys: Vec<String> = st.attachments.values().flat_map(|(_, _, a)| std::iter::once(a.blob_key.clone()).chain(a.thumb_key.clone())).collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    fn purge_rows(&self, owner: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.notes.retain(|_, (o, _)| o != owner);
        st.attachments.retain(|_, (o, _, _)| o != owner);
        st.rules.retain(|_, (o, _)| o != owner);
        st.feedback.retain(|(o, _)| o != owner);
        *st.versions.entry(owner.to_string()).or_insert(0) += 1;
        Ok(())
    }
}

impl HasTagModel for MemStore {
    fn tag_model(&self) -> TagModelService<'_> {
        TagModelService::new(&self.records, &self.models)
    }
}

impl HasRecords for MemStore {
    type R = MemRecords;
    fn records(&self) -> &MemRecords {
        &self.records
    }
}

impl NoteStore for MemStore {
    chuck_data::note_service_methods!();
    chuck_data::tag_service_methods!();
}

impl AccountRecords for MemStore {
    fn account(&self, id: &str) -> Result<Option<User>> {
        self.records.account(id)
    }
    fn insert_account_if_absent(&self, u: &User) -> Result<bool> {
        self.records.insert_account_if_absent(u)
    }
    fn update_account_profile(&self, id: &str, display_name: Option<&str>, role: &str, last_seen_at: Option<i64>) -> Result<()> {
        self.records.update_account_profile(id, display_name, role, last_seen_at)
    }
    fn set_account_status(&self, id: &str, status: &str) -> Result<bool> {
        self.records.set_account_status(id, status)
    }
    fn list_accounts(&self) -> Result<Vec<User>> {
        self.records.list_accounts()
    }
    fn remove_account(&self, id: &str) -> Result<bool> {
        self.records.remove_account(id)
    }
}

impl OwnerRecords for MemStore {
    fn usage(&self, owner: &str) -> Result<Usage> {
        self.records.usage(owner)
    }
    fn blob_keys(&self, owner: &str) -> Result<Vec<String>> {
        self.records.blob_keys(owner)
    }
    fn all_blob_keys(&self) -> Result<Vec<String>> {
        self.records.all_blob_keys()
    }
    fn purge_rows(&self, owner: &str) -> Result<()> {
        self.models.forget(owner);
        self.records.purge_rows(owner)
    }
}
