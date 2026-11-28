//! The control plane: who may use the system and in what state their account is.
//!
//! Requests are authenticated once, at the edge. `AccountService::resolve` turns the identity into a `Principal`, which
//! the data plane trusts without looking at accounts again. Account status is cached for a short time (default 30
//! seconds), so a disabled account may keep working for up to that long on another process; in the same process, changes
//! made through this service take effect at once.
//!
//! Deleting an account is a command, not a single write: the account is first marked `deleting` (refused on every
//! request), then everything it owns is purged from the data plane, and only then is the account record removed. If the
//! purge fails the account stays `deleting` and the same call can be repeated.

use chuck_core::model::Identity;
use chuck_data::blob::BlobStore;
use chuck_data::store::{Result, StoreError, User, UserSummary};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use chuck_data::owner::{OwnerRecords, Usage};

/// A complete storage backend: the data plane's `NoteStore` plus the control plane's account records plus what the data
/// plane tells the control plane about an owner. This is what the HTTP edge and the server are built on; it is implemented
/// automatically for anything that has all three.
pub trait Store: chuck_data::store::NoteStore + AccountRecords + OwnerRecords {}
impl<T: chuck_data::store::NoteStore + AccountRecords + OwnerRecords> Store for T {}

/// How long a resolved account is reused before the account record is read again.
pub const DEFAULT_ACCOUNT_CACHE: Duration = Duration::from_secs(30);
/// How often a signed-in user's "last seen" time is written.
pub const ACTIVITY_INTERVAL_SECS: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    /// Refused on every request; the data stays until the account is deleted.
    Disabled,
    /// Being erased. Refused on every request.
    Deleting,
}

impl AccountStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Active => "active",
            AccountStatus::Disabled => "disabled",
            AccountStatus::Deleting => "deleting",
        }
    }

    /// Unknown values are treated as disabled, so a bad record never grants access.
    pub fn parse(s: &str) -> Self {
        match s {
            "active" => AccountStatus::Active,
            "deleting" => AccountStatus::Deleting,
            _ => AccountStatus::Disabled,
        }
    }
}

/// Who is making a request, as the data plane needs to know it.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub id: String,
    pub display_name: Option<String>,
    pub role: Role,
    pub status: AccountStatus,
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

impl From<&User> for Principal {
    fn from(u: &User) -> Self {
        Principal {
            id: u.id.clone(),
            display_name: u.display_name.clone(),
            role: if u.role == "admin" { Role::Admin } else { Role::User },
            status: AccountStatus::parse(&u.status),
        }
    }
}

/// Thin storage for account records.
pub trait AccountRecords: Send + Sync + 'static {
    fn account(&self, id: &str) -> Result<Option<User>>;
    /// Stores a new account. False (and nothing written) if the id already exists.
    fn insert_account_if_absent(&self, u: &User) -> Result<bool>;
    /// Changes the name, role and last-seen time only, never the status, so a profile update cannot undo a concurrent
    /// disable.
    fn update_account_profile(&self, id: &str, display_name: Option<&str>, role: &str, last_seen_at: Option<i64>) -> Result<()>;
    /// False if there is no such account.
    fn set_account_status(&self, id: &str, status: &str) -> Result<bool>;
    /// In order of creation.
    fn list_accounts(&self) -> Result<Vec<User>>;
    /// False if there is no such account.
    fn remove_account(&self, id: &str) -> Result<bool>;
}

/// The data plane as the control plane sees it: usage, and erasing an owner completely (files included).
pub trait OwnerData: Send + Sync + 'static {
    fn usage(&self, owner: &str) -> Result<Usage>;
    /// Idempotent, and safe to repeat after a failure.
    fn purge(&self, owner: &str) -> Result<()>;
}

/// Erases an owner's files, then their records. Files go first so that a retry after a crash can still find their keys:
/// the account is `deleting` meanwhile, so nobody can observe a card whose file is already gone.
pub struct OwnerPurger {
    records: Arc<dyn OwnerRecords>,
    blobs: Arc<dyn BlobStore>,
}

impl OwnerPurger {
    pub fn new(records: Arc<dyn OwnerRecords>, blobs: Arc<dyn BlobStore>) -> Self {
        Self { records, blobs }
    }
}

impl OwnerData for OwnerPurger {
    fn usage(&self, owner: &str) -> Result<Usage> {
        self.records.usage(owner)
    }

    fn purge(&self, owner: &str) -> Result<()> {
        for key in self.records.blob_keys(owner)? {
            self.blobs.delete(&key).map_err(|e| StoreError::Other(format!("deleting file {key}: {e}")))?;
        }
        self.records.purge_rows(owner)
    }
}

struct Cached {
    at: Instant,
    principal: Principal,
}

pub struct AccountService {
    accounts: Arc<dyn AccountRecords>,
    data: Arc<dyn OwnerData>,
    admins: HashSet<String>,
    ttl: Duration,
    cache: Mutex<HashMap<String, Cached>>,
    now: fn() -> i64,
}

const MAX_CACHED: usize = 10_000;

impl AccountService {
    pub fn new(accounts: Arc<dyn AccountRecords>, data: Arc<dyn OwnerData>) -> Self {
        Self { accounts, data, admins: HashSet::new(), ttl: DEFAULT_ACCOUNT_CACHE, cache: Mutex::new(HashMap::new()), now: chuck_data::service::system_now }
    }

    /// Ids that are administrators. The configuration is authoritative: roles are synced from it on every sign-in.
    pub fn with_admins(mut self, admins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.admins = admins.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// For tests: use this clock for timestamps.
    pub fn with_clock(mut self, now: fn() -> i64) -> Self {
        self.now = now;
        self
    }

    pub fn has_admins(&self) -> bool {
        !self.admins.is_empty()
    }

    fn cached(&self, id: &str) -> Option<Principal> {
        let c = chuck_core::sync::lock(&self.cache);
        c.get(id).filter(|e| e.at.elapsed() < self.ttl).map(|e| e.principal.clone())
    }

    fn remember(&self, p: &Principal) {
        let mut c = chuck_core::sync::lock(&self.cache);
        if c.len() >= MAX_CACHED {
            c.clear();
        }
        c.insert(p.id.clone(), Cached { at: Instant::now(), principal: p.clone() });
    }

    fn forget(&self, id: &str) {
        chuck_core::sync::lock(&self.cache).remove(id);
    }

    /// The account for a signed-in identity. Creates it on first sight, keeps its role in step with the configured
    /// administrators, remembers the display name, and records activity (at most every few minutes; a refused account
    /// does not count as active). Reads from a short-lived cache when it can.
    pub fn resolve(&self, who: &Identity) -> Result<Principal> {
        if let Some(p) = self.cached(&who.id) {
            return Ok(p);
        }
        let t = (self.now)();
        let role = if self.admins.contains(&who.id) { "admin" } else { "user" };
        let user = match self.accounts.account(&who.id)? {
            Some(cur) => {
                let name = who.name.clone().or_else(|| cur.display_name.clone());
                let stale = cur.last_seen_at.map_or(true, |s| t - s >= ACTIVITY_INTERVAL_SECS);
                let last_seen = if cur.status == "active" && stale { Some(t) } else { cur.last_seen_at };
                if cur.role != role || name != cur.display_name || last_seen != cur.last_seen_at {
                    self.accounts.update_account_profile(&who.id, name.as_deref(), role, last_seen)?;
                }
                User { display_name: name, role: role.into(), last_seen_at: last_seen, ..cur }
            }
            None => {
                let u = User { id: who.id.clone(), display_name: who.name.clone(), role: role.into(), status: "active".into(), created_at: t, last_seen_at: Some(t) };
                if self.accounts.insert_account_if_absent(&u)? {
                    u
                } else {
                    self.accounts.account(&who.id)?.ok_or(StoreError::NotFound)?
                }
            }
        };
        let p = Principal::from(&user);
        self.remember(&p);
        Ok(p)
    }

    pub fn get(&self, id: &str) -> Result<User> {
        self.accounts.account(id)?.ok_or(StoreError::NotFound)
    }

    /// Accounts with counts and sizes from the data plane. Never any content.
    pub fn summaries(&self) -> Result<Vec<UserSummary>> {
        let mut out = Vec::new();
        for user in self.accounts.list_accounts()? {
            let u = self.data.usage(&user.id)?;
            out.push(UserSummary { user, notes: u.notes, attachments: u.attachments, storage_bytes: u.storage_bytes });
        }
        Ok(out)
    }

    /// Disables or enables an account. Takes effect at once in this process.
    pub fn set_active(&self, id: &str, active: bool) -> Result<User> {
        let cur = self.get(id)?;
        if AccountStatus::parse(&cur.status) == AccountStatus::Deleting {
            return Err(StoreError::Conflict("this account is being deleted".into()));
        }
        self.accounts.set_account_status(id, if active { "active" } else { "disabled" })?;
        self.forget(id);
        self.get(id)
    }

    /// Erases the account and everything it owns. See the module notes for the order of steps. Repeating the call after
    /// a failure finishes the job; after success the account no longer exists and the call is `NotFound`.
    pub fn delete(&self, id: &str) -> Result<()> {
        self.get(id)?;
        self.accounts.set_account_status(id, "deleting")?;
        self.forget(id);
        self.data.purge(id)?;
        self.accounts.remove_account(id)?;
        self.forget(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_is_refused() {
        assert_eq!(AccountStatus::parse("active"), AccountStatus::Active);
        assert_eq!(AccountStatus::parse("deleting"), AccountStatus::Deleting);
        assert_eq!(AccountStatus::parse("disabled"), AccountStatus::Disabled);
        assert_eq!(AccountStatus::parse("whatever"), AccountStatus::Disabled);
        assert_eq!(AccountStatus::parse(""), AccountStatus::Disabled);
    }

    #[test]
    fn principal_follows_the_account_record() {
        let u = User { id: "a".into(), display_name: Some("A".into()), role: "admin".into(), status: "deleting".into(), created_at: 1, last_seen_at: None };
        let p = Principal::from(&u);
        assert!(p.is_admin());
        assert_eq!((p.status, p.display_name.as_deref()), (AccountStatus::Deleting, Some("A")));
        let u = User { role: "something".into(), ..u };
        assert!(!Principal::from(&u).is_admin(), "an unknown role is an ordinary user");
    }
}
