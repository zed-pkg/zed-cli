//! Library core of the `zed` CLI. The binary in `main.rs` is a thin
//! dispatcher over these modules; integration tests drive them directly.

pub mod auth;
pub mod cli;
pub mod completion;
pub mod config;
pub mod flags;
pub mod interactive;
pub mod manifestless;
pub mod ops;
pub mod pack;
pub mod preflight;
pub mod r2g;
pub mod registry;
pub mod release;
pub mod store;
pub mod transaction;
pub mod update;
pub mod vcs;
