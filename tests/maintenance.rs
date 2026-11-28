//! The management plane: the orphan sweeper and the rules for administering accounts.

use chuck::admin::{sweep_orphans, AdminError, AdminService, SWEEP_GRACE_SECS};
use chuck::blob::{key_for, BlobStore, FsBlobStore, MemBlobStore};
use chuck::control::{AccountRecords, AccountService, AccountStatus, OwnerPurger, OwnerRecords, Principal, Role};
use chuck_testkit::mem::MemStore;
use chuck::store::{NewAttachment, NoteInput, NoteStore, StoreError};
use std::sync::Arc;

const NOW: i64 = 1_000_000;

fn attach(store: &MemStore, owner: &str, key: &str) {
    let n = store.create(owner, &NoteInput { body: format!("card for {key}"), ..Default::default() }).unwrap();
    let a = NewAttachment { mime: "image/jpeg".into(), bytes: 1, width: None, height: None, duration_ms: None, name: None, sha256: "x".into(), file: chuck::blob::BlobRef::from_stored(key) };
    store.add_attachment(owner, &n.id, &a).unwrap();
}

#[test]
fn only_old_unused_files_are_deleted() {
    let store = MemStore::new();
    let blobs = MemBlobStore::new();
    for k in ["used-old", "used-new", "unused-old", "unused-new"] {
        blobs.put(k, b"x").unwrap();
    }
    attach(&store, "ann", "used-old");
    attach(&store, "ann", "used-new");
    blobs.set_modified("used-old", NOW - 99_999);
    blobs.set_modified("used-new", NOW - 5);
    blobs.set_modified("unused-old", NOW - 99_999);
    blobs.set_modified("unused-new", NOW - 5);

    let r = sweep_orphans(&store, &blobs, SWEEP_GRACE_SECS, NOW).unwrap();
    assert_eq!((r.scanned, r.removed, r.too_new, r.failed), (4, 1, 1, 0));
    assert!(blobs.get("unused-old").is_err(), "an old file nothing refers to is deleted");
    for k in ["used-old", "used-new", "unused-new"] {
        assert!(blobs.get(k).is_ok(), "{k} is kept");
    }
    // a second pass finds nothing more to do; the young file ages into eligibility
    assert_eq!(sweep_orphans(&store, &blobs, SWEEP_GRACE_SECS, NOW).unwrap().removed, 0);
    assert_eq!(sweep_orphans(&store, &blobs, SWEEP_GRACE_SECS, NOW + SWEEP_GRACE_SECS + 5).unwrap().removed, 1);
}

#[test]
fn a_file_stored_again_is_not_taken_for_abandoned() {
    let store = MemStore::new();
    let blobs = MemBlobStore::new();
    blobs.put("k", b"x").unwrap();
    blobs.set_modified("k", NOW - 99_999); // stored long ago, nothing refers to it any more
    // an upload of the same content arrives: stored again, but not yet recorded on its card
    blobs.put("k", b"x").unwrap();
    let now = blobs.modified("k").unwrap().unwrap();
    assert_eq!(sweep_orphans(&store, &blobs, SWEEP_GRACE_SECS, now).unwrap().removed, 0);
    attach(&store, "ann", "k");
    assert_eq!(sweep_orphans(&store, &blobs, SWEEP_GRACE_SECS, now + 99_999).unwrap().removed, 0, "and once it is on a card it is used");
}

#[test]
fn the_folder_store_refreshes_times_lists_leftovers_and_ignores_foreign_files() {
    let dir = std::env::temp_dir().join(format!("chuck-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let blobs = FsBlobStore::open(&dir).unwrap();
    let key = key_for("ann", b"picture");
    blobs.put(&key, b"picture").unwrap();

    // pretend the file is old, then store it again
    let path = dir.join(&key);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(99_999);
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(old).unwrap();
    let before = blobs.modified(&key).unwrap().unwrap();
    blobs.put(&key, b"picture").unwrap();
    let after = blobs.modified(&key).unwrap().unwrap();
    assert!(after - before > 90_000, "storing an existing file made its time current ({before} -> {after})");

    // a leftover of an interrupted write is listed under a key that can be deleted
    let leftover = format!("{key}.tmp4242");
    std::fs::write(dir.join(&leftover), b"half").unwrap();
    // files that are not ours are never listed
    std::fs::write(dir.join("README.txt"), b"hello").unwrap();
    std::fs::write(dir.join(key.split('/').next().unwrap()).join("notes.txt"), b"hello").unwrap();

    let mut keys: Vec<String> = blobs.list().unwrap().into_iter().map(|f| f.key).collect();
    keys.sort();
    let mut expect = vec![key.clone(), leftover.clone()];
    expect.sort();
    assert_eq!(keys, expect);
    blobs.delete(&leftover).unwrap();
    assert!(!dir.join(&leftover).exists());
    assert!(blobs.delete("../../etc/passwd").is_err(), "keys are still validated");
    assert!(blobs.modified("not a key").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

fn admin_service() -> (AdminService, Arc<MemStore>, Arc<MemBlobStore>) {
    let store = Arc::new(MemStore::new());
    let blobs = Arc::new(MemBlobStore::new());
    let records: Arc<dyn OwnerRecords> = store.clone();
    let accounts: Arc<dyn AccountRecords> = store.clone();
    let purger = Arc::new(OwnerPurger::new(records.clone(), blobs.clone()));
    let service = Arc::new(AccountService::new(accounts, purger).with_cache_ttl(std::time::Duration::ZERO));
    (AdminService::new(service, records, blobs.clone()), store, blobs)
}

fn who(id: &str, role: Role) -> Principal {
    Principal { id: id.into(), display_name: None, role, status: AccountStatus::Active }
}

#[test]
fn only_administrators_may_administer_and_never_themselves() {
    let (admin, store, blobs) = admin_service();
    let (root, ann) = (who("root", Role::Admin), who("ann", Role::User));
    for id in ["root", "ann", "bob"] {
        store.insert_account_if_absent(&chuck::store::User { id: id.into(), display_name: None, role: "user".into(), status: "active".into(), created_at: 1, last_seen_at: None }).unwrap();
    }

    assert!(matches!(admin.accounts(&ann), Err(AdminError::Forbidden)));
    assert!(matches!(admin.set_active(&ann, "bob", false), Err(AdminError::Forbidden)));
    assert!(matches!(admin.delete_account(&ann, "bob"), Err(AdminError::Forbidden)));
    assert!(matches!(admin.sweep(&ann), Err(AdminError::Forbidden)));
    assert_eq!(store.account("bob").unwrap().unwrap().status, "active", "nothing happened to bob");

    assert!(matches!(admin.set_active(&root, "root", false), Err(AdminError::Invalid(_))), "no locking yourself out");
    assert!(matches!(admin.delete_account(&root, "root"), Err(AdminError::Invalid(_))), "no deleting yourself here");
    assert!(matches!(admin.set_active(&root, "nobody", false), Err(AdminError::Store(StoreError::NotFound))));

    assert_eq!(admin.set_active(&root, "bob", false).unwrap().status, "disabled");
    assert_eq!(admin.accounts(&root).unwrap().len(), 3);
    blobs.put("bob/file", b"x").unwrap();
    attach(&store, "bob", "bob/file");
    admin.delete_account(&root, "bob").unwrap();
    assert!(blobs.get("bob/file").is_err(), "deleting an account removes its files at once");
    assert_eq!(admin.accounts(&root).unwrap().len(), 2);
    assert_eq!(admin.sweep(&root).unwrap().removed, 0);
}
