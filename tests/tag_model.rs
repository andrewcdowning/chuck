//! When tag models are rebuilt. One user's edits must not make anyone else's model retrain (they used to, through a
//! single global counter), and the cache must stay bounded however many users there are.

use chuck_testkit::mem::MemStore;
use chuck::service::HasRecords;
use chuck::sqlite::SqliteStore;
use chuck::store::{NoteInput, NoteStore};
use chuck::tagmodel::{ModelCache, TagModelService, TagSource};

fn note(body: &str) -> NoteInput {
    NoteInput { body: body.into(), ..Default::default() }
}

/// The same story against any backend that is both a store and a tag source.
fn story(store: &dyn NoteStore, source: &dyn TagSource, cache: &ModelCache) {
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
    let ann_card = store.list("ann", &Default::default()).unwrap()[0].id.clone();
    store.update("ann", &ann_card, &note("now about gardening #garden")).unwrap();
    svc().suggest("bob", "milk").unwrap();
    assert_eq!(cache.trained(), 3);
    svc().suggest("ann", "lambda").unwrap();
    assert_eq!(cache.trained(), 4);
    store.delete("ann", &ann_card).unwrap();
    svc().suggest("ann", "lambda").unwrap();
    assert_eq!(cache.trained(), 5);

    // feedback and rules change suggestions but not what the model learned from
    store.record_suggestion_feedback("ann", "aws", true, 0.9).unwrap();
    store.add_rule("ann", "lambda", "aws").unwrap();
    svc().suggest("ann", "lambda").unwrap();
    assert_eq!(cache.trained(), 5);
}

#[test]
fn sqlite_retrains_only_the_owner_who_changed() {
    let store = SqliteStore::open(":memory:").unwrap();
    story(&store, &store, &ModelCache::default());
}

#[test]
fn memory_backend_retrains_only_the_owner_who_changed() {
    let store = MemStore::new();
    story(&store, store.records(), &ModelCache::default());
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
    use chuck::auth::Identity;
    use chuck::blob::MemBlobStore;
    use chuck::control::{AccountRecords, AccountService, OwnerPurger, OwnerRecords};
    use std::sync::Arc;
    let store = Arc::new(SqliteStore::open(":memory:").unwrap());
    let records: Arc<dyn OwnerRecords> = store.clone();
    let accounts_store: Arc<dyn AccountRecords> = store.clone();
    let accounts = AccountService::new(accounts_store, Arc::new(OwnerPurger::new(records, Arc::new(MemBlobStore::new()))));
    accounts.resolve(&Identity { id: "gone".into(), name: None }).unwrap();
    store.create("gone", &note("secret plans #private")).unwrap();
    store.suggest_tags("gone", "plans").unwrap();
    accounts.delete("gone").unwrap();
    let set = store.suggest_tags("gone", "plans").unwrap();
    assert_eq!(set.tagged_notes, 0, "nothing of the old account is learned from or left in a cache");
}
