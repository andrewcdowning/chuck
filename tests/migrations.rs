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
        // the schema every release before versions used, with a card, a tag and a file on it
        let mut c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        run_migrations(&mut c, &MIGRATIONS[..1]).unwrap();
        c.execute_batch(
            "PRAGMA user_version=0;
             INSERT INTO notes(id, owner, body, created_at, updated_at, position) VALUES (1, 'me', 'kept #tag', 5, 5, 3.0);
             INSERT INTO notes_fts(rowid, body) VALUES (1, 'kept #tag');
             INSERT INTO note_tags(note_id, tag) VALUES (1, 'tag');",
        )
        .unwrap();
    }
    let s = SqliteStore::open(&p).unwrap();
    let cards = s.list("me", &Default::default()).unwrap();
    assert_eq!((cards[0].body.as_str(), cards[0].tags.clone()), ("kept #tag", vec!["tag".to_string()]));
    assert_eq!(s.list("me", &chuck::store::NoteQuery { q: Some("kept".into()), ..Default::default() }).unwrap().len(), 1, "search still finds it");
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
        // a database as the release before text positions left it: step 1 only, positions are numbers
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
        let order = |c: &Connection, owner: &str| -> Vec<String> {
            let mut st = c.prepare("SELECT body FROM notes WHERE owner=?1 ORDER BY (open_todos > 0) DESC, position DESC, id DESC").unwrap();
            st.query_map([owner], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        (order(&c, "me"), order(&c, "big"))
    };

    let s = SqliteStore::open(&p).unwrap();
    let cards = |owner: &str| s.list(owner, &chuck::store::NoteQuery { limit: Some(200), ..Default::default() }).unwrap();
    let bodies = |owner: &str| cards(owner).into_iter().map(|n| n.body).collect::<Vec<_>>();
    assert_eq!(bodies("me"), before, "the board is exactly as it was");
    assert_eq!(bodies("big"), big_before);
    assert_eq!(bodies("you"), vec!["card 100".to_string()]);

    // new cards and moves work on the converted data
    let top = s.create("me", &NoteInput { body: "new".into(), ..Default::default() }).unwrap().id;
    let after = cards("me");
    let pinned = before.iter().take_while(|b| ["card 4", "card 2"].contains(&b.as_str())).count();
    assert_eq!(after.iter().position(|n| n.id == top), Some(pinned), "a new card goes above the other cards (below open todos)");
    let id_of = |body: &str| after.iter().find(|n| n.body == body).unwrap().id.clone();
    s.move_note("me", &id_of("card 7"), Some(&id_of("card 1")), Some(&id_of("card 3"))).unwrap();
    let moved: Vec<String> = bodies("me");
    let at = |b: &str| moved.iter().position(|x| x == b).unwrap();
    assert!(at("card 1") < at("card 7") && at("card 7") < at("card 3"));
    drop(s);

    let c = Connection::open(&p).unwrap();
    assert_eq!(version(&p), MIGRATIONS.len() as i64);
    let types: Vec<String> = c.prepare("SELECT DISTINCT typeof(position) FROM notes").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(types, vec!["text"]);
    let index: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='notes_owner_position'", [], |r| r.get(0)).unwrap();
    assert_eq!(index, 1);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ids_become_text_and_everything_attached_to_a_card_follows() {
    let p = temp_db("ids");
    {
        // a database as the release before text ids left it: steps 1 and 2, ids are row numbers
        let mut c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        run_migrations(&mut c, &MIGRATIONS[..2]).unwrap();
        assert_eq!(version(&p), 2);
        c.execute_batch(
            "INSERT INTO notes(id, owner, body, created_at, updated_at, position) VALUES
               (7, 'ann', 'first card about lambda #aws', 1000, 1000, 'B'),
               (9, 'ann', 'second card about milk #home', 1000, 1000, 'C'),
               (3, 'bob', 'bobs private card', 500, 500, 'B');
             INSERT INTO notes_fts(rowid, body) SELECT id, body FROM notes;
             INSERT INTO note_tags(note_id, tag) VALUES (7, 'aws'), (9, 'home'), (3, 'private');
             INSERT INTO attachments(id, owner, note_id, position, blob_key, thumb_key, sha256, mime, bytes, width, height, name, created_at)
               VALUES (5, 'ann', 7, 0, 'k1', 'k1t', 'x', 'image/png', 10, 1, 1, 'a.png', 1001),
                      (6, 'ann', 7, 1, 'k2', NULL, 'x', 'image/png', 20, 1, 1, 'b.png', 1002);
             INSERT INTO tag_rules(id, owner, pattern, tag, created_at) VALUES (2, 'ann', 'milk', 'home', 900), (1, 'ann', 'lambda', 'aws', 800);",
        )
        .unwrap();
    }

    let s = SqliteStore::open(&p).unwrap();
    let list = |o: &str| s.list(o, &Default::default()).unwrap();
    let ann = list("ann");
    assert_eq!(ann.len(), 2);
    for n in list("ann").iter().chain(list("bob").iter()) {
        assert!(chuck::domain::id::is_valid(&n.id), "{}", n.id);
    }
    let first = ann.iter().find(|n| n.body.starts_with("first")).unwrap();
    let second = ann.iter().find(|n| n.body.starts_with("second")).unwrap();
    assert!(first.id < second.id, "ids sort in creation order (ties by old id)");
    assert_eq!(first.tags, vec!["aws"]);
    assert_eq!(first.attachments.iter().map(|a| (a.name.clone().unwrap(), a.has_thumb, a.note_id == first.id)).collect::<Vec<_>>(), vec![("a.png".to_string(), true, true), ("b.png".to_string(), false, true)]);
    assert!(first.attachments.iter().all(|a| chuck::domain::id::is_valid(&a.id)));
    assert_eq!(s.rules("ann").unwrap().iter().map(|r| r.pattern.as_str()).collect::<Vec<_>>(), vec!["lambda", "milk"], "rules keep their order");
    assert!(s.rules("ann").unwrap().iter().all(|r| chuck::domain::id::is_valid(&r.id)));

    // search finds the cards through the rebuilt index, and only the owner's
    let found = |o: &str, q: &str| s.list(o, &chuck::store::NoteQuery { q: Some(q.into()), ..Default::default() }).unwrap().len();
    assert_eq!((found("ann", "lambda"), found("ann", "private"), found("bob", "private")), (1, 0, 1));
    // new work uses the new tables: ids, search, tags, deleting with cascades
    let c3 = s.create("ann", &NoteInput { body: "third card #aws".into(), ..Default::default() }).unwrap();
    assert!(c3.id > second.id);
    assert_eq!(found("ann", "third"), 1);
    s.delete("ann", &first.id).unwrap();
    assert_eq!((found("ann", "lambda"), s.attachment("ann", &first.attachments[0].id).is_err()), (0, true), "the card's search entry and files went with it");
    drop(s);

    let c = Connection::open(&p).unwrap();
    assert_eq!(version(&p), MIGRATIONS.len() as i64);
    let violations: i64 = c.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).unwrap();
    assert_eq!(violations, 0);
    let refs: Vec<String> = c.prepare("SELECT \"table\" FROM pragma_foreign_key_list('attachments')").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(refs, vec!["notes"], "files refer to the renamed table");
    let types: Vec<String> = c.prepare("SELECT typeof(id) FROM notes UNION SELECT typeof(id) FROM attachments UNION SELECT typeof(id) FROM tag_rules").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(types, vec!["text"]);
    let _ = std::fs::remove_file(&p);
}
