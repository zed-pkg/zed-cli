//! Library core of the `zed` CLI. The binary in `main.rs` is a thin
//! dispatcher over these modules; integration tests drive them directly.

pub mod cli;
pub mod config;
pub mod ops;
pub mod pack;
pub mod registry;
pub mod store;
pub mod vcs;
