//! Storage for attachment bytes. The default keeps files in a folder next to the database; a hosted deployment can
//! plug in another store (for example S3) with `Options::with_blobs`.
//!
//! Keys are `<owner hash>/<content hash>`, so identical files are stored once per user and never shared between
//! users (sharing would let one user confirm that another holds a given file).

use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};

/// Names a stored file: what the `BlobStore` is asked for. Only the media service, the record stores and the sweeper make or
/// read these; the types clients see never carry one, so how files are addressed can change without touching them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobRef(String);

impl BlobRef {
    /// The name for these bytes owned by this user: the same bytes from the same user are one file.
    pub fn for_content(owner: &str, bytes: &[u8]) -> Self {
        BlobRef(key_for(owner, bytes))
    }

    /// A name read back from a record or listing.
    pub fn from_stored(key: impl Into<String>) -> Self {
        BlobRef(key.into())
    }

    pub fn key(&self) -> &str {
        &self.0
    }
}

/// A stored file, as listed for maintenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    pub key: String,
    /// Unix seconds when the file was last written or re-stored.
    pub modified: i64,
}

pub trait BlobStore: Send + Sync + 'static {
    /// Stores the bytes under the key. Storing a key that already exists must still make its `modified` time current:
    /// the orphan sweeper relies on it so that a file being attached again right now is never taken for abandoned.
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()>;
    /// Every file in the store, including leftovers of interrupted writes (their keys end in `.tmp<number>`).
    fn list(&self) -> io::Result<Vec<BlobInfo>>;
    /// When the file was last written or re-stored, or None if there is no such file.
    fn modified(&self, key: &str) -> io::Result<Option<i64>>;
    fn get(&self, key: &str) -> io::Result<Vec<u8>>;
    /// Deleting a missing key is not an error.
    fn delete(&self, key: &str) -> io::Result<()>;
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A short version string for an attachment, derived from its storage keys (which already include the owner, so two
/// users' files never share a version). It changes when the file or its preview changes.
pub fn attachment_version(blob_key: &str, thumb_key: Option<&str>) -> String {
    sha256_hex(format!("{blob_key}|{}", thumb_key.unwrap_or("")).as_bytes())[..16].to_string()
}

/// The storage key for `bytes` owned by `owner`.
pub fn key_for(owner: &str, bytes: &[u8]) -> String {
    format!("{}/{}", &sha256_hex(owner.as_bytes())[..32], sha256_hex(bytes))
}

/// Only keys this module produces are valid: two lowercase hex segments. Anything else (`..`, `/`, absolute paths)
/// is refused before it can reach the filesystem.
fn valid_key(key: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
    // the second part may carry the `.tmp<number>` suffix of an interrupted write, so that such leftovers can be listed and removed
    let content = |b: &str| match b.split_once(".tmp") {
        Some((h, n)) => hex(h, 64) && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()),
        None => hex(b, 64),
    };
    matches!(key.split_once('/'), Some((a, b)) if hex(a, 32) && content(b))
}

fn unix(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn bad_key() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid blob key")
}

pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The default folder for a database path: `notes.db` -> `notes-media/` beside it.
    pub fn default_dir(db: &str) -> PathBuf {
        let p = Path::new(db);
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("chuck");
        p.with_file_name(format!("{stem}-media"))
    }

    fn path(&self, key: &str) -> io::Result<PathBuf> {
        if !valid_key(key) {
            return Err(bad_key());
        }
        Ok(self.root.join(key))
    }
}

impl BlobStore for FsBlobStore {
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(key)?;
        if path.exists() {
            // content-addressed: same key, same bytes. Only the modified time changes (see the trait notes).
            return std::fs::OpenOptions::new().write(true).open(&path)?.set_modified(std::time::SystemTime::now());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        // Write to a temporary name, then rename, so a crash never leaves a half-written file under the real key.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.path(key)?)
    }

    fn list(&self) -> io::Result<Vec<BlobInfo>> {
        let mut out = Vec::new();
        for dir in std::fs::read_dir(&self.root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            let Some(owner) = dir.file_name().to_str().map(String::from) else { continue };
            for f in std::fs::read_dir(dir.path())? {
                let f = f?;
                let Some(name) = f.file_name().to_str().map(String::from) else { continue };
                let key = format!("{owner}/{name}");
                if !f.file_type()?.is_file() || !valid_key(&key) {
                    continue; // not ours: never listed, so never swept
                }
                out.push(BlobInfo { key, modified: unix(f.metadata()?.modified()?) });
            }
        }
        Ok(out)
    }

    fn modified(&self, key: &str) -> io::Result<Option<i64>> {
        match std::fs::metadata(self.path(key)?) {
            Ok(m) => Ok(Some(unix(m.modified()?))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path(key)?) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_round_trip() {
        let dir = std::env::temp_dir().join(format!("chuck-blob-test-{}", std::process::id()));
        let s = FsBlobStore::open(&dir).unwrap();
        let k = key_for("alice", b"hello");
        assert_ne!(k, key_for("bob", b"hello"), "same file, different users, different keys");
        assert_eq!(k, key_for("alice", b"hello"));
        s.put(&k, b"hello").unwrap();
        s.put(&k, b"hello").unwrap();
        assert_eq!(s.get(&k).unwrap(), b"hello");
        s.delete(&k).unwrap();
        s.delete(&k).unwrap();
        assert!(s.get(&k).is_err());
        for bad in ["../etc/passwd", "/etc/passwd", "a/b", &format!("{}/../x", &k[..32]), &k.to_uppercase(), ""] {
            assert!(s.put(bad, b"x").is_err() && s.get(bad).is_err(), "{bad} accepted");
        }
        std::fs::remove_dir_all(dir).ok();
        assert_eq!(FsBlobStore::default_dir("/data/notes.db"), PathBuf::from("/data/notes-media"));
        assert_eq!(FsBlobStore::default_dir("chuck.db"), PathBuf::from("chuck-media"));
    }
}

/// Keeps files in memory. For tests and as a template; everything is lost on exit.
#[derive(Default)]
pub struct MemBlobStore {
    files: std::sync::Mutex<std::collections::HashMap<String, (Vec<u8>, i64)>>,
}

impl MemBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        chuck_core::sync::lock(&self.files).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// For tests: pretend the file was last written at this time.
    pub fn set_modified(&self, key: &str, unix_secs: i64) {
        if let Some(f) = chuck_core::sync::lock(&self.files).get_mut(key) {
            f.1 = unix_secs;
        }
    }
}

fn now_secs() -> i64 {
    unix(std::time::SystemTime::now())
}

impl BlobStore for MemBlobStore {
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        chuck_core::sync::lock(&self.files).insert(key.to_string(), (bytes.to_vec(), now_secs()));
        Ok(())
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        chuck_core::sync::lock(&self.files).get(key).map(|f| f.0.clone()).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))
    }

    fn list(&self) -> io::Result<Vec<BlobInfo>> {
        Ok(chuck_core::sync::lock(&self.files).iter().map(|(k, f)| BlobInfo { key: k.clone(), modified: f.1 }).collect())
    }

    fn modified(&self, key: &str) -> io::Result<Option<i64>> {
        Ok(chuck_core::sync::lock(&self.files).get(key).map(|f| f.1))
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        chuck_core::sync::lock(&self.files).remove(key);
        Ok(())
    }
}
