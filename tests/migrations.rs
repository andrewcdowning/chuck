//! Numbered schema migrations: versions are recorded, a failed step changes nothing, and a database from a newer
//! program is refused.

use chuck::sqlite::{run_migrations, MigrationStep, SqliteStore, MIGRATIONS};
use chuck::store::{NoteInput, NoteStore, Result, StoreError};
use rusqlite::Connection;

fn temp_db(name: &str) -> String {
    let p = std::env::temp_dir().join(format!("chuck-mig-{name}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p.to_str().unwrap().to_string()
}

fn version(path: &str) -> i64 {
    Connection::open(path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

#[test]
fn a_new_database_is_stamped_with_the_latest_version() {
    let p = temp_db("new");
    SqliteStore::open(&p).unwrap();
    assert_eq!(version(&p), MIGRATIONS.len() as i64);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn an_unversioned_database_keeps_its_data_and_gets_a_version() {
    let p = temp_db("legacy");
    {
        let s = SqliteStore::open(&p).unwrap();
        s.create("me", &NoteInput { body: "kept #tag".into(), ..Default::default() }).unwrap();
    }
    Connection::open(&p).unwrap().execute_batch("PRAGMA user_version=0;").unwrap(); // as every database before versions
    let s = SqliteStore::open(&p).unwrap();
    assert_eq!(s.list("me", &Default::default()).unwrap()[0].body, "kept #tag");
    drop(s);
    assert_eq!(version(&p), MIGRATIONS.len() as i64);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn a_database_from_a_newer_program_is_refused_and_left_alone() {
    let p = temp_db("newer");
    SqliteStore::open(&p).unwrap();
    Connection::open(&p).unwrap().execute_batch("PRAGMA user_version=99;").unwrap();
    let err = SqliteStore::open(&p).err().expect("must refuse");
    assert!(format!("{err:?}").contains("newer chuck"), "{err:?}");
    assert_eq!(version(&p), 99, "nothing was changed");
    let _ = std::fs::remove_file(&p);
}

fn create_a(c: &rusqlite::Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE a(x INTEGER); INSERT INTO a VALUES(1);")?;
    Ok(())
}

fn create_b_then_fail(c: &rusqlite::Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE b(x INTEGER);")?;
    Err(StoreError::Other("boom".into()))
}

fn create_b(c: &rusqlite::Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE b(x INTEGER);")?;
    Ok(())
}

fn tables(c: &Connection) -> Vec<String> {
    let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").unwrap();
    st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
}

#[test]
fn a_failed_step_leaves_the_previous_version_and_no_partial_changes() {
    let mut c = Connection::open_in_memory().unwrap();
    let broken: Vec<(&str, MigrationStep)> = vec![("a", create_a), ("b", create_b_then_fail)];
    assert!(run_migrations(&mut c, &broken).is_err());
    assert_eq!(c.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0)).unwrap(), 1, "step a stands");
    assert_eq!(tables(&c), vec!["a"], "step b's table was rolled back");

    // the fixed program picks up where the database was, and does not repeat step a
    let fixed: Vec<(&str, MigrationStep)> = vec![("a", create_a), ("b", create_b)];
    run_migrations(&mut c, &fixed).unwrap();
    assert_eq!(c.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0)).unwrap(), 2);
    assert_eq!(tables(&c), vec!["a", "b"]);
    assert_eq!(c.query_row::<i64, _, _>("SELECT COUNT(*) FROM a", [], |r| r.get(0)).unwrap(), 1, "step a did not run twice");
}
