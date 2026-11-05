//! Transport to Zed's upstream endpoints (spec `10-zed-upstream.md`).

pub mod client;
pub mod jwt;
pub mod models;
pub mod token;

pub use client::{ByteStream, HttpUpstreamClient, UpstreamClient};
pub use jwt::jwt_payload_bytes;
pub use models::fetch_models;
