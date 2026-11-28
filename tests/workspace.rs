//! The shape of the workspace: which crate may depend on which. The plan (docs/specs/service-layer.md) is that the planes
//! depend one way, the pure rules depend on nothing, and only the storage crate knows SQLite and only the HTTP crate knows
//! axum. Cargo enforces the allowed list below by refusing to compile a use of a crate that is not a dependency; this test
//! enforces that the list itself does not quietly grow.

use std::collections::BTreeSet;

/// crate -> the workspace crates it may depend on
const ALLOWED: &[(&str, &[&str])] = &[
    ("chuck-core", &[]),
    ("chuck-data", &["chuck-core"]),
    ("chuck-control", &["chuck-core", "chuck-data"]),
    ("chuck-mgmt", &["chuck-core", "chuck-data", "chuck-control"]),
    ("chuck-store-sqlite", &["chuck-core", "chuck-data", "chuck-control"]),
    ("chuck-testkit", &["chuck-core", "chuck-data", "chuck-control"]),
    ("chuck-http", &["chuck-core", "chuck-data", "chuck-control", "chuck-mgmt"]),
];

/// external crates a workspace crate must not use
const FORBIDDEN: &[(&str, &[&str])] = &[
    ("chuck-core", &["rusqlite", "axum", "tokio", "ureq", "jsonwebtoken", "sha2"]),
    ("chuck-data", &["rusqlite", "axum", "tokio", "ureq", "jsonwebtoken"]),
    ("chuck-control", &["rusqlite", "axum", "tokio", "ureq", "jsonwebtoken", "sha2"]),
    ("chuck-mgmt", &["rusqlite", "axum", "tokio", "ureq", "jsonwebtoken", "sha2"]),
    ("chuck-store-sqlite", &["axum", "tokio", "ureq", "jsonwebtoken"]),
    ("chuck-testkit", &["rusqlite", "axum", "tokio", "ureq", "jsonwebtoken"]),
    ("chuck-http", &["rusqlite"]),
];

/// The names in a crate's `[dependencies]` table.
fn dependencies(manifest: &str) -> BTreeSet<String> {
    let mut in_deps = false;
    let mut out = BTreeSet::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line == "[dependencies]";
        } else if in_deps && !line.is_empty() && !line.starts_with('#') {
            if let Some((name, _)) = line.split_once('=') {
                out.insert(name.trim().to_string());
            }
        }
    }
    out
}

fn manifest(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates").join(name).join("Cargo.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn crates_depend_on_each_other_only_as_planned() {
    for (name, allowed) in ALLOWED {
        let deps = dependencies(&manifest(name));
        let workspace: BTreeSet<String> = deps.iter().filter(|d| d.starts_with("chuck-")).cloned().collect();
        let expect: BTreeSet<String> = allowed.iter().map(|s| s.to_string()).collect();
        assert_eq!(workspace, expect, "{name} depends on the wrong workspace crates");
    }
}

#[test]
fn only_the_edge_and_the_backend_know_their_libraries() {
    for (name, forbidden) in FORBIDDEN {
        let deps = dependencies(&manifest(name));
        for f in *forbidden {
            assert!(!deps.contains(*f), "{name} must not depend on {f}");
        }
    }
}

#[test]
fn every_crate_in_the_directory_is_listed_here() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
    let found: BTreeSet<String> = std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    let listed: BTreeSet<String> = ALLOWED.iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(found, listed, "a crate was added or removed without updating tests/workspace.rs");
    let forbidden: BTreeSet<String> = FORBIDDEN.iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(forbidden, listed);
}

/// Where files are kept is the data plane's business. The shared types and the HTTP edge must not name storage keys or touch
/// the file store directly: they ask the media service, and what clients see carries no key.
#[test]
fn storage_keys_stay_in_the_data_plane() {
    fn sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push((p.display().to_string(), std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
    let mut files = Vec::new();
    for c in ["chuck-core", "chuck-http"] {
        sources(&root.join(c).join("src"), &mut files);
    }
    assert!(files.len() >= 10, "found only {} source files", files.len());
    for (path, text) in files {
        for banned in ["blob_key", "thumb_key", "BlobRef", "key_for(", "sha256_hex", "blobs.put(", "blobs.get(", "blobs.delete("] {
            assert!(!text.contains(banned), "{path} must not use `{banned}`: storage keys and file access belong to chuck-data");
        }
    }
}
