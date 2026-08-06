//! Library core of the `zed` CLI. The binary in `main.rs` is a thin
//! dispatcher over these modules; integration tests drive them directly.

pub mod asdf_environment;
pub mod auth;
pub mod cli;
pub mod cli_model;
pub mod completion;
pub mod config;
mod dart_wiring;
pub mod dev;
pub mod environment;
pub mod fetch;
pub mod flags;
pub mod git_submodules;
pub mod global;
pub mod inspection;
pub mod install_graph;
pub mod interactive;
pub mod lock_waiter;
pub mod managed_install;
pub mod manifestless;
pub mod mise_lock;
pub mod nix_bundle_write;
pub mod nix_export_bundle;
pub mod nix_export_plan;
#[path = "ops_entry.rs"]
pub mod ops;
pub mod pack;
pub(crate) mod pack_guard;
pub(crate) mod pack_inputs;
pub mod preflight;
pub mod project_lock;
pub mod r2g;
pub mod registry;
pub mod release;
pub mod store;
pub mod task_runtime;
pub mod transaction;
pub mod update;
pub mod vcs;

pub mod tool_versions;
