//! Inbound HTTP: server bring-up, the route table, the authentication gate,
//! body/response helpers and the embedded management page.

pub mod auth;
pub mod body;
pub mod router;
pub mod server;

/// File is `static.rs` per the module layout; `static` is a Rust keyword so the
/// module is named `static_file`.
#[path = "static.rs"]
pub mod static_file;
