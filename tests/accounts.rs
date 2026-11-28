//! Control plane behaviour: the account cache, deletion as a retryable command, and what the request edge refuses.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use chuck::auth::{Authenticator, Identity};
use chuck::blob::MemBlobStore;
use chuck::control::{AccountRecords, AccountService, AccountStatus, OwnerData, OwnerPurger, OwnerRecords, Usage};
use chuck_testkit::mem::MemStore;
use chuck::sqlite::SqliteStore;
use chuck::store::{NoteInput, NoteStore, StoreError, User};
use chuck::api;
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

fn ident(id: &str) -> Identity {
    Identity { id: id.into(), name: None }
}

/// Counts how often account records are read.
struct Counting {
    inner: Arc<dyn AccountRecords>,
    reads: AtomicUsize,
}

impl AccountRecords for Counting {
    fn account(&self, id: &str) -> chuck::store::Result<Option<User>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.account(id)
    }
    fn insert_account_if_absent(&self, u: &User) -> chuck::store::Result<bool> {
        self.inner.insert_account_if_absent(u)
    }
    fn update_account_profile(&self, id: &str, n: Option<&str>, r: &str, l: Option<i64>) -> chuck::store::Result<()> {
        self.inner.update_account_profile(id, n, r, l)
    }
    fn set_account_status(&self, id: &str, s: &str) -> chuck::store::Result<bool> {
        self.inner.set_account_status(id, s)
    }
    fn list_accounts(&self) -> chuck::store::Result<Vec<User>> {
        self.inner.list_accounts()
    }
    fn remove_account(&self, id: &str) -> chuck::store::Result<bool> {
        self.inner.remove_account(id)
    }
}

fn parts(store: &Arc<MemStore>) -> (Arc<dyn AccountRecords>, Arc<dyn OwnerRecords>) {
    (store.clone(), store.clone())
}

#[test]
fn resolved_accounts_are_cached_but_changes_here_apply_at_once() {
    let store = Arc::new(MemStore::new());
    let (accounts, records) = parts(&store);
    let counting = Arc::new(Counting { inner: accounts, reads: AtomicUsize::new(0) });
    let svc = AccountService::new(counting.clone(), Arc::new(OwnerPurger::new(records, Arc::new(MemBlobStore::new())))).with_cache_ttl(Duration::from_secs(60));

    svc.resolve(&ident("ann")).unwrap();
    let first = counting.reads.load(Ordering::SeqCst);
    for _ in 0..5 {
        assert_eq!(svc.resolve(&ident("ann")).unwrap().status, AccountStatus::Active);
    }
    assert_eq!(counting.reads.load(Ordering::SeqCst), first, "five more requests did not read the account record");

    svc.set_active("ann", false).unwrap();
    assert_eq!(svc.resolve(&ident("ann")).unwrap().status, AccountStatus::Disabled, "disabling applies at once in this process");
    svc.set_active("ann", true).unwrap();
    assert_eq!(svc.resolve(&ident("ann")).unwrap().status, AccountStatus::Active);
}

#[test]
fn a_change_made_elsewhere_applies_when_the_cache_expires() {
    let store = Arc::new(MemStore::new());
    let (accounts, records) = parts(&store);
    let purger = || Arc::new(OwnerPurger::new(records.clone(), Arc::new(MemBlobStore::new())));
    let this_process = AccountService::new(accounts.clone(), purger()).with_cache_ttl(Duration::from_millis(80));
    let another_process = AccountService::new(accounts, purger());

    assert_eq!(this_process.resolve(&ident("ann")).unwrap().status, AccountStatus::Active);
    another_process.set_active("ann", false).unwrap();
    assert_eq!(this_process.resolve(&ident("ann")).unwrap().status, AccountStatus::Active, "still cached");
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(this_process.resolve(&ident("ann")).unwrap().status, AccountStatus::Disabled, "seen once the cache expires");
}

/// A data plane whose purge fails until told otherwise.
struct Flaky {
    inner: OwnerPurger,
    broken: AtomicBool,
}

impl OwnerData for Flaky {
    fn usage(&self, owner: &str) -> chuck::store::Result<Usage> {
        self.inner.usage(owner)
    }
    fn purge(&self, owner: &str) -> chuck::store::Result<()> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(StoreError::Other("storage unavailable".into()));
        }
        self.inner.purge(owner)
    }
}

#[test]
fn a_failed_delete_leaves_a_deleting_account_that_can_be_finished() {
    let store = Arc::new(MemStore::new());
    let (accounts, records) = parts(&store);
    let flaky = Arc::new(Flaky { inner: OwnerPurger::new(records, Arc::new(MemBlobStore::new())), broken: AtomicBool::new(true) });
    let svc = AccountService::new(accounts, flaky.clone()).with_cache_ttl(Duration::ZERO);
    svc.resolve(&ident("carol")).unwrap();
    store.create("carol", &NoteInput { body: "carol's card".into(), ..Default::default() }).unwrap();

    assert!(svc.delete("carol").is_err());
    assert_eq!(svc.get("carol").unwrap().status, "deleting", "the account stays, marked deleting");
    assert_eq!(svc.resolve(&ident("carol")).unwrap().status, AccountStatus::Deleting, "and is refused meanwhile");
    assert!(matches!(svc.set_active("carol", true), Err(StoreError::Invalid(_))), "it cannot be re-enabled half way");
    assert_eq!(store.list("carol", &Default::default()).unwrap().len(), 1, "nothing was lost by the failure");

    flaky.broken.store(false, Ordering::SeqCst);
    svc.delete("carol").unwrap();
    assert!(matches!(svc.get("carol"), Err(StoreError::NotFound)));
    assert!(store.list("carol", &Default::default()).unwrap().is_empty());
}

#[test]
fn old_databases_learn_the_deleting_status() {
    let path = std::env::temp_dir().join(format!("chuck-deleting-migrate-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let p = path.to_str().unwrap();
    {
        let c = rusqlite::Connection::open(p).unwrap();
        // the table as it was before the deleting status existed
        c.execute_batch(
            "CREATE TABLE users(id TEXT PRIMARY KEY, display_name TEXT,
               role TEXT NOT NULL DEFAULT 'user' CHECK(role IN ('user','admin')),
               status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','disabled')),
               created_at INTEGER NOT NULL, last_seen_at INTEGER);
             INSERT INTO users VALUES('old', 'Old User', 'admin', 'disabled', 5, 6);",
        )
        .unwrap();
    }
    let s = SqliteStore::open(p).unwrap();
    let old = s.account("old").unwrap().unwrap();
    assert_eq!((old.display_name.as_deref(), old.role.as_str(), old.status.as_str(), old.created_at, old.last_seen_at), (Some("Old User"), "admin", "disabled", 5, Some(6)));
    assert!(s.set_account_status("old", "deleting").unwrap());
    assert_eq!(s.account("old").unwrap().unwrap().status, "deleting");
    drop(s);
    SqliteStore::open(p).unwrap(); // opening again does not rebuild or fail
    let _ = std::fs::remove_file(&path);
}

// ---- the request edge

struct CountingAuth {
    calls: Arc<AtomicUsize>,
}

impl Authenticator for CountingAuth {
    fn authenticate(&self, h: &HeaderMap) -> Option<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        h.get("x-user")?.to_str().ok().map(str::to_string)
    }
}

async fn send(router: &axum::Router, method: &str, uri: &str, user: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder().method(method).uri(uri).header("x-user", user).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    (status, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

fn app(calls: Arc<AtomicUsize>) -> (axum::Router, Arc<SqliteStore>) {
    let store = Arc::new(SqliteStore::open(":memory:").unwrap());
    let state = api::AppState::new(store.clone(), Arc::new(CountingAuth { calls }), Arc::new(MemBlobStore::new()))
        .with_admins(["root"])
        .with_account_cache(Duration::ZERO);
    (api::router(state), store)
}

#[tokio::test]
async fn a_request_is_authenticated_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (router, _) = app(calls.clone());
    let (s, _) = send(&router, "POST", "/api/notes", "ann", r#"{"body":"hello"}"#).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "identity is read once per request, not once for the edge and again for the handler");
}

#[tokio::test]
async fn refused_accounts_are_turned_away_before_the_body_is_read() {
    let (router, store) = app(Arc::new(AtomicUsize::new(0)));
    send(&router, "GET", "/api/me", "ann", "").await;
    send(&router, "GET", "/api/me", "root", "").await;

    // not an administrator: 403 even with a body that would otherwise be a 422
    let (s, m) = send(&router, "POST", "/api/admin/users/status", "ann", "not json").await;
    assert_eq!((s, m.contains("administrators only")), (StatusCode::FORBIDDEN, true));

    store.set_account_status("ann", "disabled").unwrap();
    let (s, m) = send(&router, "POST", "/api/notes", "ann", "not json").await;
    assert_eq!((s, m.contains("this account is disabled")), (StatusCode::UNAUTHORIZED, true));

    store.set_account_status("ann", "deleting").unwrap();
    let (s, m) = send(&router, "POST", "/api/notes", "ann", "not json").await;
    assert_eq!((s, m.contains("being deleted")), (StatusCode::UNAUTHORIZED, true));
    let (s, _) = send(&router, "DELETE", "/api/me", "ann", r#"{"confirm":"ann"}"#).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "even deleting yourself is refused while deletion is under way");
}

#[tokio::test]
async fn the_admin_screen_shows_deleting_accounts_and_deleting_finishes_them() {
    let (router, store) = app(Arc::new(AtomicUsize::new(0)));
    send(&router, "POST", "/api/notes", "ann", r#"{"body":"ann's card"}"#).await;
    send(&router, "GET", "/api/me", "root", "").await;
    store.set_account_status("ann", "deleting").unwrap(); // as left by a delete that failed half way
    let (_, list) = send(&router, "GET", "/api/admin/users", "root", "").await;
    assert!(list.contains("\"status\":\"deleting\"") && list.contains("\"notes\":1"), "{list}");
    let (s, _) = send(&router, "POST", "/api/admin/users/delete", "root", r#"{"id":"ann"}"#).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, list) = send(&router, "GET", "/api/admin/users", "root", "").await;
    assert!(!list.contains("\"id\":\"ann\""), "{list}");
}
