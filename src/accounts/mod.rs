//! Account pool API (spec `50-accounts-and-failover.md`).
//!
//! The store is the only owner of account state; handlers and the proxy borrow
//! snapshots from it. Implementation task fills in [`AccountStore`] internals and
//! [`status`]; the signatures below are fixed.

pub mod status;
pub mod store;

pub use status::{AccountModelState, AccountStatusReport, SchedulerStatus};
pub use store::{
    Account, AccountError, AccountSnapshot, AccountStore, AddAccountOutcome,
    COOLDOWN_AUTH_FAILURES, COOLDOWN_AUTH_LONG_S, COOLDOWN_AUTH_S, COOLDOWN_RATE_LIMIT_S,
    COOLDOWN_TRANSIENT_S, COOLDOWN_TRANSIENT_THRESHOLD, MAX_ACCOUNTS_FILE_BYTES,
    MAX_ACCOUNT_FIELD_BYTES,
};
