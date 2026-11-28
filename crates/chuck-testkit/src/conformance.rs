//! Behaviour every `NoteStore` backend must have. The cases state the contract (ordering, search, update semantics,
//! ownership, limits, accounts) in terms of the trait only, so any backend can be checked by pointing a test at it:
//!
//! ```ignore
//! fn make() -> Arc<dyn Store> { Arc::new(MyStore::open_empty()) }
//! chuck::conformance::run_all(&make);
//! ```
//!
//! `tests/store_conformance.rs` runs each case separately against SQLite. A meta-test there fails if a case is added
//! here and not listed there. Backend-specific checks (migrations, file formats) stay with the backend.

use chuck_core::model::Identity;
use chuck_data::blob::{BlobStore, MemBlobStore};
use chuck_control::{AccountRecords, AccountService, AccountStatus, OwnerPurger, OwnerRecords, Role, Store};
use chuck_data::store::*;
use std::sync::Arc;
use std::time::Duration;

/// Makes a new, empty store for one case.
pub type Make = dyn Fn() -> Arc<dyn Store>;

/// All cases: cards, tags, rules, attachments, tag suggestions and accounts. What the services provide over any backend.
pub fn run_notes(make: &Make) {
    crud_tags_and_isolation(make);
    reminders(make);
    todo_cards_pin_and_due(make);
    auto_tag_rules(make);
    attachments_are_owned_ordered_and_cascade(make);
    manual_order_within_groups(make);
    many_moves_into_one_gap(make);
    search_contract(make);
    update_semantics(make);
    paging(make);
    card_kinds(make);
    attachment_cap(make);
    ownership_is_checked_first(make);
    rule_cap(make);
    tag_suggestions_confidence_and_feedback(make);
    account_lifecycle(make);
}

/// Runs every case. Each case builds its own store through `make`.
pub fn run_all(make: &Make) {
    run_notes(make);
}

pub fn crud_tags_and_isolation(make: &Make) {
    let s = make();
    let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    let a = s.create("me", &inp("Call Bob about #project-x deadline")).unwrap();
    s.create("me", &inp("Buy milk #home")).unwrap();
    s.create("other", &inp("Secret #project-x")).unwrap();

    assert_eq!(a.tags, vec!["project-x"]);
    let r = s.list("me", &NoteQuery { q: Some("dead".into()), ..Default::default() }).unwrap();
    assert_eq!(r.len(), 1);
    let r = s.list("me", &NoteQuery { tag: Some("#HOME".into()), ..Default::default() }).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(s.tags("me").unwrap().len(), 2);

    let u = s.update("me", a.id, &inp("Now about #other")).unwrap();
    assert_eq!(u.tags, vec!["other"]);
    assert!(s.list("me", &NoteQuery { q: Some("deadline".into()), ..Default::default() }).unwrap().is_empty());
    assert!(matches!(s.delete("other", a.id), Err(StoreError::NotFound)));
    s.delete("me", a.id).unwrap();
    // injection-ish input must not error
    s.list("me", &NoteQuery { q: Some("\" OR * ( NEAR".into()), ..Default::default() }).unwrap();
}

pub fn reminders(make: &Make) {
    let s = make();
    let n = s.create("me", &NoteInput { body: "Stand up".into(), remind_at: Some(2000), repeat: Some("daily".into()), ..Default::default() }).unwrap();
    s.create("me", &NoteInput { body: "later".into(), remind_at: Some(1000), ..Default::default() }).unwrap();
    s.create("me", &NoteInput { body: "plain".into(), ..Default::default() }).unwrap();
    let q = NoteQuery { reminders: Some(true), ..Default::default() };
    let r = s.list("me", &q).unwrap();
    assert_eq!(r.iter().map(|n| n.remind_at.unwrap()).collect::<Vec<_>>(), vec![1000, 2000]);

    // body-only edit leaves the reminder alone
    let u = s.update("me", n.id, &NoteInput { body: "Stand up!".into(), ..Default::default() }).unwrap();
    assert_eq!((u.remind_at, u.repeat.as_deref()), (Some(2000), Some("daily")));
    // marking done removes it from pending; re-arming with a new time brings it back
    s.update("me", n.id, &NoteInput { body: "Stand up!".into(), reminder_done: Some(true), ..Default::default() }).unwrap();
    assert_eq!(s.list("me", &q).unwrap().len(), 1);
    s.update("me", n.id, &NoteInput { body: "Stand up!".into(), remind_at: Some(3000), ..Default::default() }).unwrap();
    assert_eq!(s.list("me", &q).unwrap().len(), 2);
    // clearing
    let c = s.update("me", n.id, &NoteInput { body: "x".into(), remind_at: Some(0), repeat: Some("".into()), ..Default::default() }).unwrap();
    assert!(c.remind_at.is_none() && c.repeat.is_none());
}

pub fn todo_cards_pin_and_due(make: &Make) {
    let s = make();
    let mk = |b: &str, due: Option<i64>| NoteInput { body: b.into(), due_at: due, ..Default::default() };
    let plain_old = s.create("me", &mk("plain old", None)).unwrap();
    let todo_nodue = s.create("me", &mk("Chores\n- [ ] sweep", None)).unwrap();
    let todo_late = s.create("me", &mk("Later\n- [ ] x", Some(2_000))).unwrap();
    let todo_soon = s.create("me", &mk("Soon\n- [ ] y", Some(1_000))).unwrap();
    let plain_new = s.create("me", &mk("plain new", None)).unwrap();
    assert_eq!(todo_soon.kind, "todo");
    assert_eq!(todo_soon.due_at, Some(1_000));

    let ids = |s: &dyn NoteStore| s.list("me", &NoteQuery::default()).unwrap().into_iter().map(|n| n.id).collect::<Vec<_>>();
    // open todos first, soonest due first, undated after dated; then the rest newest first
    assert_eq!(ids(&*s), vec![todo_soon.id, todo_late.id, todo_nodue.id, plain_new.id, plain_old.id]);

    // completing every item drops the card out of the pinned group
    s.update("me", todo_soon.id, &mk("Soon\n- [x] y", None)).unwrap();
    assert_eq!(ids(&*s), vec![todo_late.id, todo_nodue.id, plain_new.id, todo_soon.id, plain_old.id]);
    // due date is kept when absent from an update, and cleared with 0
    assert_eq!(s.get("me", todo_soon.id).unwrap().due_at, Some(1_000));
    let cleared = s.update("me", todo_soon.id, &mk("Soon\n- [x] y", Some(0))).unwrap();
    assert!(cleared.due_at.is_none());
    // reopening an item pins it again, back at its own place in the group
    s.update("me", todo_soon.id, &mk("Soon\n- [ ] y", None)).unwrap();
    assert_eq!(ids(&*s), vec![todo_soon.id, todo_late.id, todo_nodue.id, plain_new.id, plain_old.id]);
}

pub fn auto_tag_rules(make: &Make) {
    let s = make();
    let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    let old = s.create("me", &inp("Review PR-42 before standup")).unwrap();
    assert!(old.tags.is_empty());
    s.add_rule("me", "jira, PR-", "work").unwrap();
    s.add_rule("other", "milk", "private").unwrap();

    // new notes get the rule tag, and explicit tags stay
    let n = s.create("me", &inp("Fix PR-7 #urgent")).unwrap();
    assert_eq!(n.tags, vec!["urgent", "work"]);
    // existing notes are tagged on demand
    assert_eq!(s.apply_rules("me").unwrap(), 1);
    assert_eq!(s.get("me", old.id).unwrap().tags, vec!["work"]);
    assert_eq!(s.apply_rules("me").unwrap(), 0);
    // owner isolation
    assert_eq!(s.rules("me").unwrap().len(), 1);
    assert!(s.create("me", &inp("milk")).unwrap().tags.is_empty());
    let r = s.rules("me").unwrap();
    assert!(matches!(s.delete_rule("other", r[0].id), Err(StoreError::NotFound)));
    s.delete_rule("me", r[0].id).unwrap();
    assert!(s.rules("me").unwrap().is_empty());
}

pub fn tag_suggestions_confidence_and_feedback(make: &Make) {
    let s = make();
    let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    for i in 0..20 {
        s.create("me", &inp(&format!("Deploy the lambda stack to staging, bucket policy v{i} #aws"))).unwrap();
        s.create("me", &inp(&format!("Buy milk and garden soil, plumber visit v{i} #home"))).unwrap();
    }
    s.create("other", &inp("lambda lambda lambda #secret")).unwrap();

    let set = s.suggest_tags("me", "investigate lambda deploy timeout").unwrap();
    assert_eq!((set.tagged_notes, set.calibrated), (40, true));
    let sug = set.suggestions;
    assert_eq!(sug[0].tag, "aws");
    assert!(sug[0].confidence >= 0.7 && sug[0].tier == "high", "{:?}", sug[0]);
    assert!(!sug[0].why.is_empty());
    assert!(sug.iter().all(|x| x.tag != "secret"));
    // written tags and rule-added tags are never suggested
    assert!(s.suggest_tags("me", "lambda #aws").unwrap().suggestions.iter().all(|x| x.tag != "aws"));
    s.add_rule("me", "lambda", "aws").unwrap();
    assert!(s.suggest_tags("me", "lambda").unwrap().suggestions.iter().all(|x| x.tag != "aws"));
    assert!(s.suggest_tags("me", "zzz qqq").unwrap().suggestions.is_empty());
    // an owner with no tagged notes gets an empty set that says so
    let none = s.suggest_tags("nobody", "anything at all").unwrap();
    assert!(none.suggestions.is_empty() && none.tagged_notes == 0 && !none.calibrated);

    // dismissals lower a tag's confidence
    let s2 = make();
    for i in 0..20 {
        s2.create("me", &inp(&format!("Deploy the lambda stack to staging, bucket policy v{i} #aws"))).unwrap();
        s2.create("me", &inp(&format!("Buy milk and garden soil, plumber visit v{i} #home"))).unwrap();
    }
    let before = s2.suggest_tags("me", "lambda bucket").unwrap().suggestions[0].confidence;
    for _ in 0..6 {
        s2.record_suggestion_feedback("me", "aws", false, 0.9).unwrap();
    }
    let after = s2.suggest_tags("me", "lambda bucket").unwrap().suggestions.iter().find(|x| x.tag == "aws").map_or(0.0, |x| x.confidence);
    assert!(after < before, "{after} !< {before}");

    // the cached model follows edits: a brand new tag becomes suggestable once notes use it
    for i in 0..4 {
        s2.create("me", &inp(&format!("Sourdough starter feeding schedule {i} #baking"))).unwrap();
    }
    assert!(s2.suggest_tags("me", "feed the sourdough starter").unwrap().suggestions.iter().any(|x| x.tag == "baking"));
}

pub fn attachments_are_owned_ordered_and_cascade(make: &Make) {
    let s = make();
    let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    let n = s.create("alice", &inp("holiday")).unwrap();
    let other = s.create("bob", &inp("bob's")).unwrap();
    assert_eq!(n.kind, "note");
    let new = |key: &str| NewAttachment {
        mime: "image/jpeg".into(),
        bytes: 10,
        width: Some(640),
        height: Some(480),
        duration_ms: None,
        name: Some("a.jpg".into()),
        blob_key: key.into(),
        sha256: "x".into(),
    };
    let a1 = s.add_attachment("alice", n.id, &new("k1")).unwrap();
    let a2 = s.add_attachment("alice", n.id, &new("k2")).unwrap();
    let note = s.get("alice", n.id).unwrap();
    assert_eq!(note.kind, "media");
    assert_eq!(note.attachments.iter().map(|a| a.id).collect::<Vec<_>>(), vec![a1.id, a2.id]);
    // keys never serialize
    let json = serde_json::to_string(&note).unwrap();
    assert!(!json.contains("k1") && json.contains("\"has_thumb\":false"));

    // another user can neither attach to, read, change nor delete alice's attachments
    assert!(matches!(s.add_attachment("bob", n.id, &new("kb")), Err(StoreError::NotFound)));
    assert!(matches!(s.attachment("bob", a1.id), Err(StoreError::NotFound)));
    assert!(matches!(s.set_attachment_thumb("bob", a1.id, "t"), Err(StoreError::NotFound)));
    assert!(matches!(s.delete_attachment("bob", a1.id), Err(StoreError::NotFound)));
    assert!(!s.blob_referenced("bob", "k1").unwrap());
    assert!(s.get("bob", other.id).unwrap().attachments.is_empty());

    // previews
    assert_eq!(s.set_attachment_thumb("alice", a1.id, "t1").unwrap(), None);
    assert_eq!(s.set_attachment_thumb("alice", a1.id, "t2").unwrap(), Some("t1".to_string()));
    assert!(s.attachment("alice", a1.id).unwrap().has_thumb);
    assert!(s.blob_referenced("alice", "t2").unwrap() && !s.blob_referenced("alice", "t1").unwrap());

    // delete one, then the note: the rest cascade
    let gone = s.delete_attachment("alice", a2.id).unwrap();
    assert_eq!(gone.blob_key, "k2");
    assert!(!s.blob_referenced("alice", "k2").unwrap());
    s.delete("alice", n.id).unwrap();
    assert!(matches!(s.attachment("alice", a1.id), Err(StoreError::NotFound)));
    assert!(!s.blob_referenced("alice", "k1").unwrap());
}

pub fn manual_order_within_groups(make: &Make) {
    let s = make();
    let mk = |b: &str, due: Option<i64>| NoteInput { body: b.into(), due_at: due, ..Default::default() };
    let a = s.create("me", &mk("a", None)).unwrap().id;
    let b = s.create("me", &mk("b", None)).unwrap().id;
    let c = s.create("me", &mk("c", None)).unwrap().id;
    let t1 = s.create("me", &mk("t1\n- [ ] x", Some(100))).unwrap().id;
    let t2 = s.create("me", &mk("t2\n- [ ] x", Some(200))).unwrap().id;
    let other = s.create("you", &mk("theirs", None)).unwrap().id;
    let ids = |s: &dyn NoteStore| s.list("me", &NoteQuery::default()).unwrap().into_iter().map(|n| n.id).collect::<Vec<_>>();
    assert_eq!(ids(&*s), vec![t1, t2, c, b, a], "new cards on top, todos slotted by due date");

    // move within the rest: a between c and b; then to the top; then to the bottom
    s.move_note("me", a, Some(c), Some(b)).unwrap();
    assert_eq!(ids(&*s), vec![t1, t2, c, a, b]);
    s.move_note("me", b, None, Some(c)).unwrap();
    assert_eq!(ids(&*s), vec![t1, t2, b, c, a]);
    s.move_note("me", b, Some(a), None).unwrap();
    assert_eq!(ids(&*s), vec![t1, t2, c, a, b]);
    // within the todo group, manual order replaces due-date order...
    s.move_note("me", t1, Some(t2), None).unwrap();
    assert_eq!(ids(&*s), vec![t2, t1, c, a, b]);
    // ...and a new todo is still slotted by due date among them: before the first one due later
    let t3 = s.create("me", &mk("t3\n- [ ] x", Some(150))).unwrap().id;
    assert_eq!(ids(&*s), vec![t3, t2, t1, c, a, b], "t2 (due 200) is the first due after 150");
    let t4 = s.create("me", &mk("t4\n- [ ] x", None)).unwrap().id;
    assert_eq!(ids(&*s)[..4], [t3, t2, t1, t4], "undated todos go last in the group");

    // across the group boundary, someone else's card, bad neighbours
    assert!(matches!(s.move_note("me", a, Some(t1), None), Err(StoreError::Invalid(_))));
    assert!(matches!(s.move_note("me", t1, Some(c), None), Err(StoreError::Invalid(_))));
    assert!(matches!(s.move_note("me", a, Some(other), None), Err(StoreError::NotFound)));
    assert!(matches!(s.move_note("you", a, Some(b), None), Err(StoreError::NotFound)));
    assert!(matches!(s.move_note("me", a, None, None), Err(StoreError::Invalid(_))));
    assert!(matches!(s.move_note("me", a, Some(a), None), Err(StoreError::Invalid(_))));
    assert!(matches!(s.move_note("me", a, Some(b), Some(c)), Err(StoreError::Invalid(_))), "above must come before below");
    assert_eq!(s.list("you", &NoteQuery::default()).unwrap().len(), 1);

    // completing a todo drops it into the rest at its own position; reopening brings it back to its place
    s.update("me", t1, &mk("t1\n- [x] x", None)).unwrap();
    assert!(!ids(&*s)[..3].contains(&t1));
    s.update("me", t1, &mk("t1\n- [ ] x", None)).unwrap();
    assert_eq!(ids(&*s)[..4], [t3, t2, t1, t4]);
}

pub fn many_moves_into_one_gap(make: &Make) {
    let s = make();
    let mk = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    let x = s.create("me", &mk("x")).unwrap().id;
    let y = s.create("me", &mk("y")).unwrap().id;
    let movers: Vec<i64> = (0..80).map(|i| s.create("me", &mk(&format!("m{i}"))).unwrap().id).collect();
    // keep pushing cards between y and the previous mover: halves the gap every time, more often than any fixed-size
    // number could stand
    let mut below = x;
    for m in &movers {
        s.move_note("me", *m, Some(y), Some(below)).unwrap();
        below = *m;
    }
    let ids: Vec<i64> = s.list("me", &NoteQuery { limit: Some(200), ..Default::default() }).unwrap().into_iter().map(|n| n.id).collect();
    let mut expect = vec![y];
    expect.extend(movers.iter().rev());
    expect.push(x);
    assert_eq!(ids, expect);
}

/// The account lifecycle through the control plane: first sight provisions, roles follow the configuration, disabling and
/// deleting work, deleting erases everything the account owns (files included) and nothing of anyone else's, and a
/// deleted account signs in again as a new, empty one.
pub fn account_lifecycle(make: &Make) {
    let s = make();
    let (accounts, blobs) = accounts_for(&s, ["alice"]);
    let ident = |id: &str, name: Option<&str>| Identity { id: id.into(), name: name.map(String::from) };

    // first sight creates the account; later calls keep it, update the name, follow the admin list
    let p = accounts.resolve(&ident("alice", None)).unwrap();
    assert_eq!((p.role, p.status, p.display_name.clone()), (Role::Admin, AccountStatus::Active, None));
    assert!(accounts.get("alice").unwrap().last_seen_at.is_some());
    let p = accounts.resolve(&ident("alice", Some("Alice A"))).unwrap();
    assert_eq!(p.display_name.as_deref(), Some("Alice A"));
    let (not_admin, _) = accounts_for(&s, Vec::<String>::new());
    let p = not_admin.resolve(&ident("alice", None)).unwrap();
    assert_eq!((p.role, p.display_name.as_deref()), (Role::User, Some("Alice A")), "role follows config; a missing name keeps the old one");
    accounts.resolve(&ident("bob", None)).unwrap();
    assert_eq!(accounts.summaries().unwrap().iter().map(|u| u.user.id.as_str()).collect::<Vec<_>>(), vec!["alice", "bob"]);

    // disabling is visible on the next resolve and does not count as activity
    assert_eq!(accounts.set_active("bob", false).unwrap().status, "disabled");
    assert_eq!(accounts.resolve(&ident("bob", None)).unwrap().status, AccountStatus::Disabled);
    s.update_account_profile("bob", None, "user", Some(1)).unwrap();
    assert_eq!(accounts.get("bob").unwrap().status, "disabled", "a profile update never changes the status");
    assert_eq!(accounts.set_active("bob", true).unwrap().status, "active");
    assert!(matches!(accounts.set_active("nobody", false), Err(StoreError::NotFound)));

    // everything an account owns goes with it, and nothing of anyone else's
    let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    let mine = s.create("alice", &inp("ALICEWORD plan #secret")).unwrap();
    let bobs = s.create("bob", &inp("BOBWORD plan #other")).unwrap();
    s.add_rule("alice", "ALICEWORD", "secret").unwrap();
    s.record_suggestion_feedback("alice", "secret", true, 0.9).unwrap();
    for k in ["alice/f1", "alice/t1", "bob/f1"] {
        blobs.put(k, b"x").unwrap();
    }
    let new = |k: &str| NewAttachment { mime: "image/jpeg".into(), bytes: 100, width: Some(1), height: Some(1), duration_ms: None, name: None, blob_key: k.into(), sha256: "x".into() };
    let a = s.add_attachment("alice", mine.id, &new("alice/f1")).unwrap();
    s.set_attachment_thumb("alice", a.id, "alice/t1").unwrap();
    s.add_attachment("bob", bobs.id, &new("bob/f1")).unwrap();
    let summary = accounts.summaries().unwrap();
    let alice = summary.iter().find(|u| u.user.id == "alice").unwrap();
    assert_eq!((alice.notes, alice.attachments, alice.storage_bytes), (1, 1, 100));

    accounts.delete("alice").unwrap();
    assert!(blobs.get("alice/f1").is_err() && blobs.get("alice/t1").is_err(), "her files are deleted");
    assert!(blobs.get("bob/f1").is_ok(), "his are not");
    assert!(s.list("alice", &NoteQuery { q: Some("ALICEWORD".into()), ..Default::default() }).unwrap().is_empty(), "search index cleaned");
    assert!(s.tags("alice").unwrap().is_empty() && s.rules("alice").unwrap().is_empty() && s.feedback_events("alice").unwrap().is_empty());
    assert!(matches!(s.attachment("alice", a.id), Err(StoreError::NotFound)));
    assert_eq!(accounts.summaries().unwrap().len(), 1);
    assert_eq!(s.list("bob", &NoteQuery::default()).unwrap().len(), 1, "bob untouched");
    assert!(s.blob_referenced("bob", "bob/f1").unwrap());
    assert!(matches!(accounts.delete("alice"), Err(StoreError::NotFound)));
    // signing in again starts a fresh account
    assert_eq!(accounts.resolve(&ident("alice", None)).unwrap().status, AccountStatus::Active);
    assert!(s.list("alice", &NoteQuery::default()).unwrap().is_empty());
}

fn accounts_for(s: &Arc<dyn Store>, admins: impl IntoIterator<Item = impl Into<String>>) -> (AccountService, Arc<MemBlobStore>) {
    let blobs = Arc::new(MemBlobStore::new());
    let records: Arc<dyn OwnerRecords> = s.clone();
    let data = Arc::new(OwnerPurger::new(records, blobs.clone()));
    let accounts: Arc<dyn AccountRecords> = s.clone();
    // a zero cache lifetime, so every call reads the stored record
    (AccountService::new(accounts, data).with_admins(admins).with_cache_ttl(Duration::ZERO), blobs)
}

/// Search contract: case-insensitive (Unicode too), each word is a prefix of a word in the note, all words must match
/// in any order, a match never starts mid-word, a blank query filters nothing, and no input is an error.
pub fn search_contract(make: &Make) {
    let s = make();
    let mk = |o: &str, b: &str| s.create(o, &NoteInput { body: b.into(), ..Default::default() }).unwrap().id;
    let report = mk("me", "Deadline for the Quarterly Report #work");
    let street = mk("me", "dead end street");
    let school = mk("me", "École élémentaire en été");
    let theirs = mk("you", "Deadline secret");
    let find = |o: &str, q: &str| {
        let mut v: Vec<i64> = s.list(o, &NoteQuery { q: Some(q.into()), ..Default::default() }).unwrap().into_iter().map(|n| n.id).collect();
        v.sort();
        v
    };
    let mut both = vec![report, street];
    both.sort();
    assert_eq!(find("me", "DEADLINE"), vec![report], "case-insensitive");
    assert_eq!(find("me", "dead"), both, "prefix of a word");
    assert!(find("me", "line").is_empty(), "never from the middle of a word");
    assert_eq!(find("me", "report dead"), vec![report], "all words, any order");
    assert!(find("me", "dead zebra").is_empty(), "every word must match");
    assert_eq!(find("me", "ÉCOLE"), vec![school], "case-insensitive beyond ASCII");
    assert_eq!(find("me", "   ").len(), 3, "a blank query filters nothing");
    for q in ["\"", "\"\"", "* OR (", "a\"b", "NEAR(", "-", "^x", "dead:", "AND", "'; DROP TABLE notes; --"] {
        s.list("me", &NoteQuery { q: Some(q.into()), ..Default::default() }).unwrap();
    }
    assert!(find("you", "street").is_empty(), "other owners' cards are never searched");
    assert_eq!(find("you", "dead"), vec![theirs]);
    let tagged = s.list("me", &NoteQuery { q: Some("dead".into()), tag: Some("#Work".into()), ..Default::default() }).unwrap();
    assert_eq!(tagged.iter().map(|n| n.id).collect::<Vec<_>>(), vec![report], "search and tag filters combine");
}

/// Update semantics, as documented on `NoteInput`: an absent field is unchanged, a value of 0 or less (or an empty
/// repeat) clears it, and changing the reminder time re-arms a finished reminder while keeping the same time does not.
pub fn update_semantics(make: &Make) {
    let s = make();
    let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    let n = s.create("me", &NoteInput { body: "a".into(), remind_at: Some(100), repeat: Some("weekly".into()), due_at: Some(50), ..Default::default() }).unwrap();
    let u = s.update("me", n.id, &inp("b")).unwrap();
    assert_eq!((u.body.as_str(), u.remind_at, u.repeat.as_deref(), u.due_at, u.reminder_done), ("b", Some(100), Some("weekly"), Some(50), false));
    assert!(u.updated_at >= u.created_at);

    s.update("me", n.id, &NoteInput { body: "b".into(), reminder_done: Some(true), ..Default::default() }).unwrap();
    let same = s.update("me", n.id, &NoteInput { body: "b".into(), remind_at: Some(100), ..Default::default() }).unwrap();
    assert!(same.reminder_done, "the same time does not re-arm");
    let moved = s.update("me", n.id, &NoteInput { body: "b".into(), remind_at: Some(200), ..Default::default() }).unwrap();
    assert!(!moved.reminder_done && moved.remind_at == Some(200), "a new time re-arms");

    let cleared = s.update("me", n.id, &NoteInput { body: "c".into(), remind_at: Some(-5), due_at: Some(-1), repeat: Some(String::new()), ..Default::default() }).unwrap();
    assert_eq!((cleared.remind_at, cleared.due_at, cleared.repeat), (None, None, None));

    assert!(matches!(s.update("me", 9_999_999, &inp("x")), Err(StoreError::NotFound)));
    assert!(matches!(s.update("you", n.id, &inp("x")), Err(StoreError::NotFound)));
    assert_eq!(s.get("me", n.id).unwrap().body, "c", "a refused update changes nothing");
}

/// Paging: the default page is 50, a page never exceeds 200, and pages follow one another without gaps or repeats,
/// newest card first.
pub fn paging(make: &Make) {
    let s = make();
    let ids: Vec<i64> = (0..210).map(|i| s.create("me", &NoteInput { body: format!("n{i}"), ..Default::default() }).unwrap().id).collect();
    let page = |limit: Option<u32>, offset: Option<u32>| s.list("me", &NoteQuery { limit, offset, ..Default::default() }).unwrap();
    assert_eq!(page(None, None).len(), 50);
    assert_eq!(page(Some(1000), None).len(), 200);
    let mut seen = Vec::new();
    for offset in [0, 70, 140, 210] {
        seen.extend(page(Some(70), Some(offset)).into_iter().map(|n| n.id));
    }
    let newest_first: Vec<i64> = ids.iter().rev().copied().collect();
    assert_eq!(seen, newest_first);
    assert!(page(Some(10), Some(500)).is_empty());
}

fn new_attachment(key: &str) -> NewAttachment {
    NewAttachment { mime: "image/jpeg".into(), bytes: 10, width: Some(1), height: Some(1), duration_ms: None, name: None, blob_key: key.into(), sha256: "x".into() }
}

/// Card kind comes from content: reminder beats todo beats media beats note.
pub fn card_kinds(make: &Make) {
    let s = make();
    let mk = |b: &str, remind: Option<i64>| s.create("me", &NoteInput { body: b.into(), remind_at: remind, ..Default::default() }).unwrap();
    assert_eq!(mk("plain", None).kind, "note");
    assert_eq!(mk("- [ ] a", None).kind, "todo");
    assert_eq!(mk("- [x] done already", None).kind, "todo", "a finished checklist is still a todo card");
    assert_eq!(mk("call", Some(100)).kind, "reminder");
    assert_eq!(mk("- [ ] a", Some(100)).kind, "reminder");
    let with_file = |b: &str, remind: Option<i64>, key: &str| {
        let n = mk(b, remind);
        s.add_attachment("me", n.id, &new_attachment(key)).unwrap();
        s.get("me", n.id).unwrap().kind
    };
    assert_eq!(with_file("pic", None, "k1"), "media");
    assert_eq!(with_file("- [ ] a", None, "k2"), "todo");
    assert_eq!(with_file("call", Some(100), "k3"), "reminder");
}

/// A card holds at most `MAX_ATTACHMENTS_PER_NOTE` files. Going over is a client error (`Invalid`), not a server fault,
/// and removing one makes room again.
pub fn attachment_cap(make: &Make) {
    let s = make();
    let n = s.create("me", &NoteInput { body: "many".into(), ..Default::default() }).unwrap();
    let first = s.add_attachment("me", n.id, &new_attachment("k0")).unwrap();
    for i in 1..MAX_ATTACHMENTS_PER_NOTE {
        s.add_attachment("me", n.id, &new_attachment(&format!("k{i}"))).unwrap();
    }
    assert!(matches!(s.add_attachment("me", n.id, &new_attachment("over")), Err(StoreError::Invalid(_))));
    assert!(!s.blob_referenced("me", "over").unwrap());
    s.delete_attachment("me", first.id).unwrap();
    s.add_attachment("me", n.id, &new_attachment("again")).unwrap();
    let other = s.create("me", &NoteInput { body: "other card".into(), ..Default::default() }).unwrap();
    s.add_attachment("me", other.id, &new_attachment("k-other")).unwrap();
}

/// Someone else's card is `NotFound` for every operation, whatever else is wrong with the request. Ownership is checked
/// first, so a refusal never reveals whether the card exists or whether the arguments were valid.
pub fn ownership_is_checked_first(make: &Make) {
    let s = make();
    let inp = |b: &str| NoteInput { body: b.into(), ..Default::default() };
    let a = s.create("me", &inp("a")).unwrap().id;
    let b = s.create("me", &inp("b")).unwrap().id;
    let nf = |r: Result<()>, what: &str| assert!(matches!(r, Err(StoreError::NotFound)), "{what}");
    nf(s.move_note("you", a, None, None).map(|_| ()), "move without neighbours");
    nf(s.move_note("you", a, Some(a), None).map(|_| ()), "move next to itself");
    nf(s.move_note("you", a, Some(b), Some(b)).map(|_| ()), "move between swapped neighbours");
    nf(s.get("you", a).map(|_| ()), "get");
    nf(s.update("you", a, &inp("")).map(|_| ()), "update with an empty body");
    nf(s.delete("you", a), "delete");
    nf(s.add_attachment("you", a, &new_attachment("k")).map(|_| ()), "attach");
    nf(s.get("me", 9_999_999).map(|_| ()), "a card that does not exist");
    assert_eq!(s.list("me", &NoteQuery::default()).unwrap().len(), 2);
}

/// A user may have at most `MAX_RULES` auto-tag rules. Going over is a client error (`Invalid`), and deleting one
/// makes room.
pub fn rule_cap(make: &Make) {
    let s = make();
    let first = s.add_rule("me", "k0", "t0").unwrap();
    for i in 1..chuck_data::service::MAX_RULES {
        s.add_rule("me", &format!("k{i}"), "t").unwrap();
    }
    assert!(matches!(s.add_rule("me", "over", "t"), Err(StoreError::Invalid(_))));
    assert_eq!(s.rules("me").unwrap().len(), chuck_data::service::MAX_RULES);
    s.add_rule("you", "mine", "t").unwrap();
    s.delete_rule("me", first.id).unwrap();
    s.add_rule("me", "again", "t").unwrap();
}
