//! Transport to Zed's upstream endpoints (spec `10-zed-upstream.md`).

pub mod client;
pub mod jwt;
pub mod models;
pub mod token;

pub use client::{
    classify_attempt, parse_status_failed, AttemptFailure, ByteStream, HttpUpstreamClient,
    UpstreamClient, UpstreamRequest, UpstreamResponse, UpstreamStream, MAX_ATTEMPTS,
};
pub use jwt::jwt_payload_bytes;
pub use jwt::{jwt_exp, jwt_payload};
pub use models::fetch_models;
pub use token::{mint_token, organization_id, TokenCache};
