//! Runs the backend-neutral store contract (`src/conformance.rs`) against SQLite, one test per case.

use chuck_testkit::conformance as c;
use chuck::sqlite::SqliteStore;
use chuck::control::Store;
use std::sync::Arc;

fn make() -> Arc<dyn Store> {
    Arc::new(SqliteStore::open(":memory:").unwrap())
}

macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $( #[test] fn $name() { c::$name(&make) } )*
        const LISTED: &[&str] = &[$(stringify!($name)),*];
    };
}

cases!(crud_tags_and_isolation, reminders, todo_cards_pin_and_due, auto_tag_rules, tag_suggestions_confidence_and_feedback, attachments_are_owned_ordered_and_cascade, manual_order_within_groups, many_moves_into_one_gap_renumber, account_lifecycle, search_contract, update_semantics, paging, card_kinds, attachment_cap, ownership_is_checked_first, rule_cap);

#[test]
fn every_conformance_case_is_listed() {
    let src = include_str!("../crates/chuck-testkit/src/conformance.rs");
    let defined: Vec<&str> = src
        .lines()
        .filter_map(|l| l.strip_prefix("pub fn "))
        .filter_map(|l| l.split('(').next())
        .filter(|n| !n.starts_with("run_"))
        .collect();
    assert!(defined.len() >= 10, "found only {} cases", defined.len());
    for n in &defined {
        assert!(LISTED.contains(n), "case {n} in src/conformance.rs is not listed in tests/store_conformance.rs");
    }
    for n in LISTED {
        assert!(defined.contains(n), "{n} is listed here but not defined");
    }
}

#[test]
fn run_all_runs_without_error() {
    c::run_all(&make);
}
