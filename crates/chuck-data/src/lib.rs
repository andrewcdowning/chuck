//! The data plane's services over thin storage traits: cards, tags, rules, files and tag suggestions. A backend
//! implements `records::NoteRecords`, `tagmodel::TagSource` and `owner::OwnerRecords`; every rule users can observe lives
//! in `service::NoteService`, `tagmodel::TagModelService` and `chuck_core::domain`.

pub mod blob;
pub mod ids;
pub mod owner;
pub mod records;
pub mod service;
pub mod store;
pub mod tagmodel;

/// Re-exported so the macros in `service` and `tagmodel` resolve their paths through `$crate`.
pub use chuck_core::tagging;
