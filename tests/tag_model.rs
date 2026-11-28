//! When tag models are rebuilt. One user's edits must not make anyone else's model retrain (they used to, through a
//! single global counter), and the cache must stay bounded however many users there are.

use chuck::mem::MemRecords;
use chuck::sqlite::SqliteStore;
use chuck::store::{NoteInput, NoteStore};
use chuck::tagmodel::{ModelCache, TagModelService, TagSource};

fn note(body: &str) -> NoteInput {
    NoteInput { body: body.into(), ..Default::default() }
}

/// The same story against any backend that is both a store and a tag source.
fn story(store: &dyn NoteStore, source: &'static dyn TagSource, cache: &'static ModelCache) {
    for i in 0..4 {
        store.create("ann", &note(&format!("deploy the lambda stack {i} #aws"))).unwrap();
        store.create("bob", &note(&format!("buy milk and eggs {i} #home"))).unwrap();
    }
    let svc = || TagModelService::new(source, cache);
    svc().suggest("ann", "lambda").unwrap();
    svc().suggest("bob", "milk").unwrap();
    assert_eq!(cache.trained(), 2, "one model per user");

    svc().suggest("ann", "lambda again").unwrap();
    svc().suggest("bob", "milk again").unwrap();
    assert_eq!(cache.trained(), 2, "nothing changed, so nothing is retrained");

    store.create("bob", &note("another #home card")).unwrap();
    svc().suggest("ann", "lambda").unwrap();
    assert_eq!(cache.trained(), 2, "bob's new card does not touch ann's model");
    svc().suggest("bob", "milk").unwrap();
    assert_eq!(cache.trained(), 3, "bob's own model is rebuilt");

    // editing and deleting count as changes too, and only for the owner
    let ann_card = store.list("ann", &Default::default()).unwrap()[0].id;
    store.update("ann", ann_card, &note("now about gardening #garden")).unwrap();
    svc().suggest("bob", "milk").unwrap();
    assert_eq!(cache.trained(), 3);
    svc().suggest("ann", "lambda").unwrap();
    assert_eq!(cache.trained(), 4);
    store.delete("ann", ann_card).unwrap();
    svc().suggest("ann", "lambda").unwrap();
    assert_eq!(cache.trained(), 5);

    // feedback and rules change suggestions but not what the model learned from
    store.record_suggestion_feedback("ann", "aws", true, 0.9).unwrap();
    store.add_rule("ann", "lambda", "aws").unwrap();
    svc().suggest("ann", "lambda").unwrap();
    assert_eq!(cache.trained(), 5);
}

fn leak<T>(v: T) -> &'static T {
    Box::leak(Box::new(v))
}

#[test]
fn sqlite_retrains_only_the_owner_who_changed() {
    let store: &'static SqliteStore = leak(SqliteStore::open(":memory:").unwrap());
    story(store, store, leak(ModelCache::default()));
}

#[test]
fn memory_backend_retrains_only_the_owner_who_changed() {
    let records: &'static MemRecords = leak(MemRecords::new());
    story(&MemOverRecords(records), records, leak(ModelCache::default()));
}

/// A `NoteStore` for the notes part only, over a shared `MemRecords`, so the test can also use it as a `TagSource`.
struct MemOverRecords(&'static MemRecords);

impl chuck::service::HasRecords for MemOverRecords {
    type R = MemRecords;
    fn records(&self) -> &MemRecords {
        self.0
    }
}

impl NoteStore for MemOverRecords {
    chuck::note_service_methods!();
    fn suggest_tags(&self, _: &str, _: &str) -> chuck::store::Result<chuck::tagging::SuggestionSet> {
        unreachable!()
    }
    fn record_suggestion_feedback(&self, owner: &str, tag: &str, accepted: bool, confidence: f64) -> chuck::store::Result<()> {
        self.0.record_feedback(owner, tag, accepted, confidence, 0)
    }
    fn feedback_events(&self, owner: &str) -> chuck::store::Result<Vec<chuck::store::FeedbackEvent>> {
        self.0.feedback_events(owner)
    }
    fn touch_user(&self, _: &str, _: Option<&str>, _: bool) -> chuck::store::Result<chuck::store::User> {
        unreachable!()
    }
    fn list_users(&self) -> chuck::store::Result<Vec<chuck::store::UserSummary>> {
        unreachable!()
    }
    fn set_user_active(&self, _: &str, _: bool) -> chuck::store::Result<chuck::store::User> {
        unreachable!()
    }
    fn delete_user(&self, _: &str) -> chuck::store::Result<Vec<String>> {
        unreachable!()
    }
}

#[test]
fn the_cache_is_bounded_and_keeps_recently_used_models() {
    let store = SqliteStore::open(":memory:").unwrap();
    let cache = ModelCache::new(3);
    let svc = || TagModelService::new(&store, &cache);
    for u in ["u1", "u2", "u3"] {
        store.create(u, &note(&format!("{u} writes about things #t"))).unwrap();
        svc().suggest(u, "things").unwrap();
    }
    assert_eq!(cache.len(), 3);
    svc().suggest("u1", "things").unwrap(); // u1 is now the most recently used; u2 is the oldest
    store.create("u4", &note("u4 writes about things #t")).unwrap();
    svc().suggest("u4", "things").unwrap();
    assert_eq!(cache.len(), 3, "never more than the capacity");
    let before = cache.trained();
    svc().suggest("u1", "things").unwrap();
    svc().suggest("u3", "things").unwrap();
    assert_eq!(cache.trained(), before, "u1 and u3 are still cached");
    svc().suggest("u2", "things").unwrap();
    assert_eq!(cache.trained(), before + 1, "u2 was the one dropped");
}

#[test]
fn deleting_an_account_forgets_its_model() {
    let store = SqliteStore::open(":memory:").unwrap();
    store.touch_user("gone", None, false).unwrap();
    store.create("gone", &note("secret plans #private")).unwrap();
    store.suggest_tags("gone", "plans").unwrap();
    store.delete_user("gone").unwrap();
    let set = store.suggest_tags("gone", "plans").unwrap();
    assert_eq!(set.tagged_notes, 0, "nothing of the old account is learned from or left in a cache");
}
