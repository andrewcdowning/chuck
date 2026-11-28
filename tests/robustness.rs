//! A panic while serving one request must not break the ones after it. Locks used to be taken with `lock().unwrap()`, which
//! panics for every later caller once a holder has panicked ("poisoned"), so one bad request kept failing until a restart.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chuck::blob::MemBlobStore;
use chuck::records::{NoteRecords, RecordTx};
use chuck::sqlite::SqliteStore;
use chuck::store::{NoteInput, NoteStore, StoreError};
use chuck::{api, auth::Authenticator};
use chuck_testkit::mem::{MemRecords, MemStore};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use tower::ServiceExt;

fn a_unit_of_work_that_panics_half_way<R: NoteRecords>(records: &R) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let _ = records.transact(|tx: &mut dyn RecordTx| -> Result<(), StoreError> {
            tx.insert_rule("me", "01ARZ3NDEKTSV4RRFFQ69G5FAV", "half", "done", 0)?;
            panic!("a request went wrong half way through");
        });
    }));
    assert!(outcome.is_err(), "the closure panicked, as arranged");
}

fn rules<R: NoteRecords>(records: &R) -> usize {
    records.transact(|tx| Ok(tx.rules("me")?.len())).unwrap()
}

#[test]
fn sqlite_keeps_working_after_a_panic_and_forgets_the_unfinished_work() {
    let store = SqliteStore::open(":memory:").unwrap();
    store.add_rule("me", "kept", "tag").unwrap();
    a_unit_of_work_that_panics_half_way(&store);
    assert_eq!(rules(&store), 1, "the half-written rule was rolled back, the earlier one is intact");
    store.add_rule("me", "another", "tag").unwrap();
    store.create("me", &NoteInput { body: "still works #fine".into(), ..Default::default() }).unwrap();
    assert_eq!(store.list("me", &Default::default()).unwrap().len(), 1);
    assert_eq!(store.suggest_tags("me", "works").unwrap().tagged_notes, 1, "tag suggestions work too");
}

#[test]
fn the_memory_backend_keeps_working_after_a_panic_and_forgets_the_unfinished_work() {
    let records = MemRecords::new();
    a_unit_of_work_that_panics_half_way(&records);
    assert_eq!(rules(&records), 0);
    let store = MemStore::new();
    store.add_rule("me", "kept", "tag").unwrap();
    assert_eq!(store.rules("me").unwrap().len(), 1);
}

/// Signs everyone in, but panics for the user called "boom".
struct Panicky;

impl Authenticator for Panicky {
    fn authenticate(&self, h: &HeaderMap) -> Option<String> {
        let user = h.get("x-user")?.to_str().ok()?.to_string();
        if user == "boom" {
            panic!("the authenticator fell over");
        }
        Some(user)
    }
}

async fn status(router: &axum::Router, user: &str) -> StatusCode {
    let req = Request::builder().method("GET").uri("/api/notes").header("x-user", user).body(Body::empty()).unwrap();
    router.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn a_panic_in_one_request_is_a_500_and_the_next_request_is_fine() {
    let state = api::AppState::new(Arc::new(SqliteStore::open(":memory:").unwrap()), Arc::new(Panicky), Arc::new(MemBlobStore::new()));
    let router = api::router(state);
    assert_eq!(status(&router, "ann").await, StatusCode::OK);
    assert_eq!(status(&router, "boom").await, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(status(&router, "ann").await, StatusCode::OK, "the server carries on");
    assert_eq!(status(&router, "boom").await, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(status(&router, "bob").await, StatusCode::OK);
}
