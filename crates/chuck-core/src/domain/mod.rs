//! The rules of the app as pure functions: no database, no HTTP, no clock. Storage backends and services call these so
//! that every backend behaves the same. Nothing in this module may depend on `store`, `sqlite` or `api`; `store`
//! re-exports what its callers used to find there.

pub mod card;
pub mod input;
pub mod order;
pub mod schedule;
pub mod search;

#[cfg(test)]
mod tests {
    /// The domain rules must not know about storage or HTTP, or a second backend could not reuse them.
    #[test]
    fn domain_does_not_depend_on_storage_or_http() {
        let files = [
            ("card.rs", include_str!("card.rs")),
            ("input.rs", include_str!("input.rs")),
            ("order.rs", include_str!("order.rs")),
            ("schedule.rs", include_str!("schedule.rs")),
            ("search.rs", include_str!("search.rs")),
        ];
        for (name, src) in files {
            for banned in ["crate::model", "crate::sqlite", "crate::api", "rusqlite", "axum", "crate::auth", "SystemTime"] {
                assert!(!src.contains(banned), "domain/{name} must not use {banned}");
            }
        }
    }
}
