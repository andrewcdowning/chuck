//! Storage for attachment bytes. The default keeps files in a folder next to the database; a hosted deployment can
//! plug in another store (for example S3) with `Options::with_blobs`.
//!
//! Keys are `<owner hash>/<content hash>`, so identical files are stored once per user and never shared between
//! users (sharing would let one user confirm that another holds a given file).

use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};

pub trait BlobStore: Send + Sync + 'static {
    fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()>;
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
    matches!(key.split_once('/'), Some((a, b)) if hex(a, 32) && hex(b, 64))
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
            return Ok(()); // content-addressed: same key, same bytes
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
