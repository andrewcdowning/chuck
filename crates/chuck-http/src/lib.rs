//! The HTTP edge: routers for the three planes, authentication, and the request-to-`Principal` step that every route
//! shares. Depends on all the services and on no storage backend.

pub mod api;
pub mod assets;
pub mod auth;
