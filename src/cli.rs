//! Command-line helpers that run against a local database instead of starting the server.

use crate::{sqlite::SqliteStore, tagging};

const USAGE: &str = "usage: chuck eval-tags [--db PATH] [--owner ID]

Leave-one-out evaluation of tag suggestions on your own notes: how often the hidden tag is suggested,
precision and recall at each confidence cutoff, and whether the stated confidence matches reality.
--db defaults to $CHUCK_DB or chuck.db. --owner is only needed when the database holds several owners.";

/// Returns the process exit code.
pub fn eval_tags(args: &[String]) -> i32 {
    let mut db = std::env::var("CHUCK_DB").unwrap_or_else(|_| "chuck.db".into());
    let mut owner: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => match it.next() {
                Some(v) => db = v.clone(),
                None => return usage_error("--db needs a value"),
            },
            "--owner" => match it.next() {
                Some(v) => owner = Some(v.clone()),
                None => return usage_error("--owner needs a value"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other => return usage_error(&format!("unknown argument `{other}`")),
        }
    }
    if !std::path::Path::new(&db).exists() {
        eprintln!("database not found: {db}\n{USAGE}");
        return 2;
    }
    let store = match SqliteStore::open(&db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot open {db}: {e:?}");
            return 2;
        }
    };
    let owners = match store.owners() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("cannot read {db}: {e:?}");
            return 2;
        }
    };
    let owner = match (owner, owners.as_slice()) {
        (Some(o), _) => o,
        (None, [(only, _)]) => only.clone(),
        (None, []) => {
            eprintln!("{db} has no notes yet");
            return 2;
        }
        (None, many) => {
            eprintln!("{db} holds several owners; pick one with --owner:");
            for (o, n) in many {
                eprintln!("  {o}  ({n} notes)");
            }
            return 2;
        }
    };
    match store.training_docs(&owner) {
        Ok(docs) => {
            println!("owner: {owner}\n");
            print!("{}", tagging::evaluate(&docs));
            0
        }
        Err(e) => {
            eprintln!("cannot read notes: {e:?}");
            2
        }
    }
}

fn usage_error(msg: &str) -> i32 {
    eprintln!("{msg}\n{USAGE}");
    2
}
