//! Account pool API (spec `50-accounts-and-failover.md`).
//!
//! The store is the only owner of account state; handlers and the proxy borrow
//! snapshots from it. Implementation task fills in [`AccountStore`] internals and
//! [`status`]; the signatures below are fixed.

pub mod status;
pub mod store;

pub use status::AccountModelState;
pub use store::{AccountError, AccountSnapshot, AccountStore};
