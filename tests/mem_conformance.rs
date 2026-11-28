//! The same behaviour contract as SQLite, run against the in-memory reference backend. Every card, tag, rule and
//! attachment case must pass here too, which shows that all of that behaviour lives in `NoteService`, not in a backend.

use chuck_testkit::conformance as c;
use chuck_testkit::mem::MemStore;
use chuck::control::Store;
use std::sync::Arc;

fn make() -> Arc<dyn Store> {
    Arc::new(MemStore::new())
}

macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $( #[test] fn $name() { c::$name(&make) } )*
        const LISTED: &[&str] = &[$(stringify!($name)),*];
    };
}

cases!(
    crud_tags_and_isolation,
    reminders,
    todo_cards_pin_and_due,
    auto_tag_rules,
    attachments_are_owned_ordered_and_cascade,
    manual_order_within_groups,
    many_moves_into_one_gap,
    search_contract,
    update_semantics,
    paging,
    card_kinds,
    attachment_cap,
    ownership_is_checked_first,
    rule_cap,
    ids_are_opaque_and_never_reused,
    tag_suggestions_confidence_and_feedback,
    account_lifecycle
);

#[test]
fn every_case_runs_here() {
    let src = include_str!("../crates/chuck-testkit/src/conformance.rs");
    for n in src.lines().filter_map(|l| l.strip_prefix("pub fn ")).filter_map(|l| l.split('(').next()) {
        if matches!(n, "run_all" | "run_notes") {
            continue;
        }
        assert!(LISTED.contains(&n), "case {n} is not run against MemStore");
    }
}

#[test]
fn run_notes_passes() {
    c::run_notes(&make);
}

#[test]
fn a_failed_unit_of_work_changes_nothing() {
    use chuck::records::{NoteRecords, RecordTx};
    use chuck::store::StoreError;
    let r = chuck_testkit::mem::MemRecords::new();
    let out: Result<(), StoreError> = r.transact(|tx: &mut dyn RecordTx| {
        tx.insert_rule("me", "01ARZ3NDEKTSV4RRFFQ69G5FAV", "a", "b", 0)?;
        Err(StoreError::Invalid("stop".into()))
    });
    assert!(out.is_err());
    r.transact(|tx| {
        assert!(tx.rules("me")?.is_empty(), "the rule written before the error was rolled back");
        Ok(())
    })
    .unwrap();
}
