//! Chuck as a library. The `chuck` binary is a thin wrapper over `run`. To plug in your own authentication
//! (or storage) without forking, depend on this crate and write your own `main`; see `examples/custom_auth.rs`.

pub mod api;
pub mod auth;
pub mod blob;
pub mod cli;
pub mod conformance;
pub mod domain;
pub mod export;
pub mod media;
pub mod mem;
pub mod records;
pub mod service;
pub mod sqlite;
pub mod store;
pub mod tagging;
pub mod zip;

use std::sync::Arc;

pub struct Options {
    pub db: String,
    pub addr: String,
    /// Replaces the default SQLite store when set.
    pub store: Option<Arc<dyn store::NoteStore>>,
    /// Replaces the default attachment folder when set (for example with an S3 store).
    pub blobs: Option<Arc<dyn blob::BlobStore>>,
    /// Folder for attachments; defaults to `<database name>-media` beside the database.
    pub media_dir: Option<String>,
    /// Administrators: owner ids allowed to manage accounts (`CHUCK_ADMINS`, comma separated). In local mode the one
    /// local user is always an administrator.
    pub admins: Vec<String>,
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

    pub fn with_store(mut self, store: Arc<dyn store::NoteStore>) -> Self {
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
    let store: Arc<dyn store::NoteStore> = match opts.store {
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
    let app = api::router(api::AppState { store, auth: cfg.auth, blobs, admins: Arc::new(admins) });
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
    println!("chuck listening on http://{addr} (auth: {})", cfg.names.join(","));
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}
