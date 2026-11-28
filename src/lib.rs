//! Chuck as a library. The `chuck` binary is a thin wrapper over `run`. To plug in your own authentication
//! (or storage) without forking, depend on this crate and write your own `main`; see `examples/custom_auth.rs`.

pub mod cli;

// The server is a workspace of crates (see docs/specs/service-layer.md). These re-exports keep the module paths the
// library has always had, so `chuck::store::NoteStore` and `chuck::api::router` still work.
pub use chuck_control as control;
pub use chuck_core::{domain, export, media, tagging, zip};
pub use chuck_data::{blob, note_service_methods, owner, records, service, store, tag_service_methods, tagmodel};
pub use chuck_http::{api, auth};
pub use chuck_mgmt as admin;
pub use chuck_store_sqlite as sqlite;

use std::sync::Arc;

pub struct Options {
    pub db: String,
    pub addr: String,
    /// Replaces the default SQLite store when set.
    pub store: Option<Arc<dyn control::Store>>,
    /// Replaces the default attachment folder when set (for example with an S3 store).
    pub blobs: Option<Arc<dyn blob::BlobStore>>,
    /// Folder for attachments; defaults to `<database name>-media` beside the database.
    pub media_dir: Option<String>,
    /// Administrators: owner ids allowed to manage accounts (`CHUCK_ADMINS`, comma separated). In local mode the one
    /// local user is always an administrator.
    pub admins: Vec<String>,
    /// How long a signed-in account is reused before its record is read again (`CHUCK_ACCOUNT_CACHE_SECS`, default 30).
    /// Disabling an account takes effect at once in this process and within this time anywhere else.
    pub account_cache: std::time::Duration,
    /// How often unused files are deleted (`CHUCK_SWEEP_MINUTES`, default 10; 0 turns the scheduled sweep off).
    pub sweep_interval: Option<std::time::Duration>,
    pub providers: auth::Providers,
}

impl Options {
    /// `CHUCK_DB` (default `chuck.db`), `CHUCK_ADDR` (default `127.0.0.1:3000`), `CHUCK_MEDIA_DIR` (default: a
    /// `<database name>-media` folder beside the database).
    pub fn from_env() -> Self {
        Self {
            db: std::env::var("CHUCK_DB").unwrap_or_else(|_| "chuck.db".into()),
            addr: std::env::var("CHUCK_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into()),
            store: None,
            blobs: None,
            media_dir: std::env::var("CHUCK_MEDIA_DIR").ok().filter(|v| !v.is_empty()),
            admins: std::env::var("CHUCK_ADMINS").unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            account_cache: std::env::var("CHUCK_ACCOUNT_CACHE_SECS").ok().and_then(|v| v.trim().parse().ok()).map(std::time::Duration::from_secs).unwrap_or(control::DEFAULT_ACCOUNT_CACHE),
            sweep_interval: match std::env::var("CHUCK_SWEEP_MINUTES").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
                Some(0) => None,
                Some(m) => Some(std::time::Duration::from_secs(m * 60)),
                None => Some(std::time::Duration::from_secs(600)),
            },
            providers: auth::Providers::default(),
        }
    }

    /// Makes `CHUCK_AUTH=<name>` select this provider. `build` runs at startup and should read its own settings
    /// and return a clear error for bad configuration.
    pub fn with_provider<F>(mut self, name: &str, build: F) -> Self
    where
        F: Fn() -> Result<Arc<dyn auth::Authenticator>, String> + Send + Sync + 'static,
    {
        self.providers.register(name, Box::new(build)).expect("invalid provider registration");
        self
    }

    pub fn with_store(mut self, store: Arc<dyn control::Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_admins(mut self, admins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.admins = admins.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_blobs(mut self, blobs: Arc<dyn blob::BlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
    }
}

pub async fn run(opts: Options) -> Result<(), String> {
    let store: Arc<dyn control::Store> = match opts.store {
        Some(s) => s,
        None => Arc::new(sqlite::SqliteStore::open(&opts.db).map_err(|e| format!("open database: {e:?}"))?),
    };
    let blobs: Arc<dyn blob::BlobStore> = match opts.blobs {
        Some(b) => b,
        None => {
            let dir = opts.media_dir.map(std::path::PathBuf::from).unwrap_or_else(|| blob::FsBlobStore::default_dir(&opts.db));
            Arc::new(blob::FsBlobStore::open(&dir).map_err(|e| format!("open media folder {}: {e}", dir.display()))?)
        }
    };
    let cfg = auth::from_env(&opts.providers).map_err(|e| format!("auth config error: {e}"))?;

    // Refuse to serve unauthenticated on a non-loopback address unless the operator says so explicitly.
    let addr = opts.addr;
    let loopback = addr.parse::<std::net::SocketAddr>().map(|a| a.ip().is_loopback()).unwrap_or(false);
    if cfg.is_local_only && !loopback {
        if std::env::var("CHUCK_ALLOW_INSECURE").as_deref() == Ok("1") {
            eprintln!("WARNING: serving without authentication on {addr} (CHUCK_ALLOW_INSECURE=1)");
        } else {
            return Err(format!("refusing to listen on {addr} without authentication: set CHUCK_AUTH or CHUCK_ALLOW_INSECURE=1"));
        }
    }

    let mut admins: std::collections::HashSet<String> = opts.admins.into_iter().collect();
    if cfg.is_local_only {
        admins.insert("local".into());
    }
    if admins.is_empty() {
        eprintln!("note: no administrators configured (CHUCK_ADMINS); nobody can manage accounts");
    }
    let state = api::AppState::new(store, cfg.auth, blobs).with_admins(admins).with_account_cache(opts.account_cache);
    if let Some(every) = opts.sweep_interval {
        spawn_sweeper(state.admin.clone(), every);
    }
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
    println!("chuck listening on http://{addr} (auth: {})", cfg.names.join(","));
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

/// Deletes unused files now and then, for as long as the server runs. The first pass waits a little so that start-up is
/// not slowed down.
fn spawn_sweeper(admin: Arc<admin::AdminService>, every: std::time::Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30).min(every)).await;
        loop {
            let a = admin.clone();
            match tokio::task::spawn_blocking(move || a.sweep_unattended(admin::SWEEP_GRACE_SECS)).await {
                Ok(Ok(r)) if r.removed > 0 || r.failed > 0 => println!("swept unused files: {} removed, {} failed", r.removed, r.failed),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("sweeping unused files failed: {e:?}"),
                Err(e) => eprintln!("sweeping unused files failed: {e}"),
            }
            tokio::time::sleep(every).await;
        }
    });
}
