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

#[test]
fn positions_become_text_and_every_board_keeps_its_order() {
    let p = temp_db("positions");
    let (before, big_before) = {
        // a database as the previous release left it: step 1 only, positions are numbers
        let mut c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        run_migrations(&mut c, &MIGRATIONS[..1]).unwrap();
        assert_eq!(version(&p), 1);
        let ins = |owner: &str, id: i64, open: i64, pos: f64| {
            c.execute(
                "INSERT INTO notes(id, owner, body, created_at, updated_at, open_todos, position) VALUES(?1, ?2, ?3, 1, 1, ?4, ?5)",
                rusqlite::params![id, owner, format!("card {id}"), open, pos],
            )
            .unwrap();
        };
        // positions interleave between the groups, include ties and a value that was nearly out of precision
        for (id, open, pos) in [(1, 0, 5.0), (2, 1, 2.0), (3, 0, 4.0), (4, 1, 9.0), (5, 0, 4.0), (6, 0, 4.000000001), (7, 0, 0.0)] {
            ins("me", id, open, pos);
        }
        ins("you", 100, 0, 1.0);
        for i in 0..200 {
            ins("big", 1000 + i, 0, (i as f64) * 1.5);
        }
        let order = |c: &Connection, owner: &str| -> Vec<i64> {
            let mut st = c.prepare("SELECT id FROM notes WHERE owner=?1 ORDER BY (open_todos > 0) DESC, position DESC, id DESC").unwrap();
            st.query_map([owner], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        (order(&c, "me"), order(&c, "big"))
    };

    let s = SqliteStore::open(&p).unwrap();
    let ids = |owner: &str| s.list(owner, &chuck::store::NoteQuery { limit: Some(200), ..Default::default() }).unwrap().into_iter().map(|n| n.id).collect::<Vec<_>>();
    assert_eq!(ids("me"), before, "the board is exactly as it was");
    assert_eq!(ids("big"), big_before);
    assert_eq!(ids("you"), vec![100]);

    // new cards and moves work on the converted data
    let top = s.create("me", &NoteInput { body: "new".into(), ..Default::default() }).unwrap().id;
    assert_eq!(ids("me").iter().position(|i| *i == top), Some(before.iter().take_while(|i| [4, 2].contains(i)).count()), "a new card goes above the other cards (below open todos)");
    s.move_note("me", 7, Some(1), Some(3)).unwrap();
    let after = ids("me");
    let at = |id: i64| after.iter().position(|i| *i == id).unwrap();
    assert!(at(1) < at(7) && at(7) < at(3));
    drop(s);

    let c = Connection::open(&p).unwrap();
    assert_eq!(version(&p), 2);
    let types: Vec<String> = c.prepare("SELECT DISTINCT typeof(position) FROM notes").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(types, vec!["text"]);
    let index: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='notes_owner_position'", [], |r| r.get(0)).unwrap();
    assert_eq!(index, 1);
    let _ = std::fs::remove_file(&p);
}
