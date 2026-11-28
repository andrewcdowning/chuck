//! The shared vocabulary and the rules of the app, with no storage, no HTTP and no clock: the plain types (`model`),
//! the pure rules (`domain`), the tag-learning maths (`tagging`), and byte-level helpers (`media`, `export`, `zip`).

pub mod domain;
pub mod export;
pub mod media;
pub mod model;
pub mod tagging;
pub mod zip;
