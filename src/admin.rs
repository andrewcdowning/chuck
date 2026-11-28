//! The management plane: operating the system. Administrators manage accounts (never content) and run maintenance.
//!
//! This layer depends on the control plane's `AccountService` and on what the data plane exposes in `OwnerRecords` and
//! `BlobStore`. It never reads cards. Rules that protect accounts (an administrator cannot disable or delete their own
//! account here) live in this service, not in the HTTP handlers, so every host that serves it applies them.

use crate::blob::BlobStore;
use crate::control::{AccountService, OwnerRecords, Principal};
use crate::service::system_now;
use crate::store::{StoreError, User, UserSummary};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;

/// A file must have gone this many seconds without being written or re-stored before it can be swept. It covers the time
/// between storing an upload and recording it on its card.
pub const SWEEP_GRACE_SECS: i64 = 600;

#[derive(Debug)]
pub enum AdminError {
    /// The caller is not an administrator.
    Forbidden,
    /// The request is refused (for example, acting on one's own account).
    Invalid(String),
    Store(StoreError),
}

impl From<StoreError> for AdminError {
    fn from(e: StoreError) -> Self {
        AdminError::Store(e)
    }
}

pub type AdminResult<T> = std::result::Result<T, AdminError>;

/// What a sweep did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    /// Files looked at.
    pub scanned: usize,
    /// Unused files deleted.
    pub removed: usize,
    /// Unused files left alone because they were written too recently.
    pub too_new: usize,
    /// Files that could not be deleted (they are tried again next time).
    pub failed: usize,
}

/// Deletes files that no card refers to and that have not been written for `grace_secs`.
///
/// Order matters: the files are listed first and the references read second, so a file stored after the listing is never
/// considered, and one stored before it but not yet attached is protected by its fresh modified time. The modified time
/// is checked once more just before each delete.
pub fn sweep_orphans(records: &dyn OwnerRecords, blobs: &dyn BlobStore, grace_secs: i64, now: i64) -> crate::store::Result<SweepReport> {
    let files = blobs.list().map_err(|e| StoreError::Other(format!("listing files: {e}")))?;
    let referenced: HashSet<String> = records.all_blob_keys()?.into_iter().collect();
    let mut report = SweepReport { scanned: files.len(), ..Default::default() };
    for f in files {
        if referenced.contains(&f.key) {
            continue;
        }
        if now - f.modified < grace_secs {
            report.too_new += 1;
            continue;
        }
        // Someone may have stored the same content again since the listing.
        match blobs.modified(&f.key) {
            Ok(Some(m)) if now - m < grace_secs => {
                report.too_new += 1;
                continue;
            }
            Ok(None) => continue,
            Err(_) => {
                report.failed += 1;
                continue;
            }
            Ok(Some(_)) => {}
        }
        match blobs.delete(&f.key) {
            Ok(()) => report.removed += 1,
            Err(e) => {
                eprintln!("could not delete unused file {}: {e}", f.key);
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

pub struct AdminService {
    accounts: Arc<AccountService>,
    records: Arc<dyn OwnerRecords>,
    blobs: Arc<dyn BlobStore>,
    now: fn() -> i64,
}

impl AdminService {
    pub fn new(accounts: Arc<AccountService>, records: Arc<dyn OwnerRecords>, blobs: Arc<dyn BlobStore>) -> Self {
        Self { accounts, records, blobs, now: system_now }
    }

    /// For tests: use this clock.
    pub fn with_clock(mut self, now: fn() -> i64) -> Self {
        self.now = now;
        self
    }

    fn require_admin(actor: &Principal) -> AdminResult<()> {
        if actor.is_admin() {
            Ok(())
        } else {
            Err(AdminError::Forbidden)
        }
    }

    /// Accounts with counts and sizes. Never any content.
    pub fn accounts(&self, actor: &Principal) -> AdminResult<Vec<UserSummary>> {
        Self::require_admin(actor)?;
        Ok(self.accounts.summaries()?)
    }

    /// Disables or enables another account. An administrator cannot do this to their own account.
    pub fn set_active(&self, actor: &Principal, id: &str, active: bool) -> AdminResult<User> {
        Self::require_admin(actor)?;
        if id == actor.id {
            return Err(AdminError::Invalid("you cannot disable your own account".into()));
        }
        Ok(self.accounts.set_active(id, active)?)
    }

    /// Deletes another account and everything it owns. To delete one's own account, use the self-service command.
    pub fn delete_account(&self, actor: &Principal, id: &str) -> AdminResult<()> {
        Self::require_admin(actor)?;
        if id == actor.id {
            return Err(AdminError::Invalid("to delete your own account use Delete my account".into()));
        }
        Ok(self.accounts.delete(id)?)
    }

    /// Deletes unused files on request.
    pub fn sweep(&self, actor: &Principal) -> AdminResult<SweepReport> {
        Self::require_admin(actor)?;
        Ok(self.sweep_unattended(SWEEP_GRACE_SECS)?)
    }

    /// The scheduled form of the sweep, which has no human caller.
    pub fn sweep_unattended(&self, grace_secs: i64) -> crate::store::Result<SweepReport> {
        sweep_orphans(&*self.records, &*self.blobs, grace_secs, (self.now)())
    }
}
