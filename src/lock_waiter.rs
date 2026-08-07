//! Compatibility re-export for callers that previously used
//! `zed_cli::lock_waiter::LockWaiter`.
//!
//! The implementation now lives in the independently packageable
//! `zed-lock` workspace crate.

pub use zed_lock::LockWaiter;
