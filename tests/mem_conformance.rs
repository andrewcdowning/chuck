//! The same behaviour contract as SQLite, run against the in-memory reference backend. Every card, tag, rule and
//! attachment case must pass here too, which shows that all of that behaviour lives in `NoteService`, not in a backend.

use chuck::conformance as c;
use chuck::mem::MemStore;
use chuck::store::NoteStore;
use std::sync::Arc;

fn make() -> Arc<dyn NoteStore> {
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
    many_moves_into_one_gap_renumber,
    search_contract,
    update_semantics,
    paging,
    card_kinds,
    attachment_cap,
    ownership_is_checked_first,
    rule_cap,
    tag_suggestions_confidence_and_feedback,
    account_lifecycle
);

#[test]
fn every_case_runs_here() {
    let src = include_str!("../src/conformance.rs");
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
    let r = chuck::mem::MemRecords::new();
    let out: Result<(), StoreError> = r.transact(|tx: &mut dyn RecordTx| {
        tx.insert_rule("me", "a", "b", 0)?;
        Err(StoreError::Invalid("stop".into()))
    });
    assert!(out.is_err());
    r.transact(|tx| {
        assert!(tx.rules("me")?.is_empty(), "the rule written before the error was rolled back");
        Ok(())
    })
    .unwrap();
}
