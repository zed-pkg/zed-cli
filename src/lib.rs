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
pub mod external_subcommands;
pub mod fetch;
pub mod flags;
pub mod git_submodules;
pub mod global;
pub mod install_graph;
pub mod interactive;
pub mod lock_waiter;
pub mod managed_install;
pub mod manifestless;
#[path = "mise_export.rs"]
mod mise_export_impl;
pub mod mise_export {
    use std::path::{Component, Path};

    use anyhow::{Context, Result, ensure};

    pub use super::mise_export_impl::{
        MiseExportAction, MiseExportMode, MiseExportReport, print_export,
    };

    const EXPORT_STATE_PATH: &str = ".zed/mise-export-state.json";

    /// Render, verify, or write one project-local mise projection.
    ///
    /// The ownership sidecar is never accepted as an input plan, including
    /// through portable case aliases or an in-project symlink.
    pub fn export_mise(
        cwd: &Path,
        plan_arg: &Path,
        output_arg: &Path,
        mode: MiseExportMode,
    ) -> Result<MiseExportReport> {
        reject_reserved_state_plan(cwd, plan_arg)?;
        super::mise_export_impl::export_mise(cwd, plan_arg, output_arg, mode)
    }

    fn reject_reserved_state_plan(cwd: &Path, plan_arg: &Path) -> Result<()> {
        let root = cwd
            .canonicalize()
            .with_context(|| format!("failed to resolve project root {}", cwd.display()))?;

        let lexical_relative = if plan_arg.is_absolute() {
            plan_arg.strip_prefix(&root).ok()
        } else {
            Some(plan_arg)
        };
        if let Some(relative) = lexical_relative {
            ensure_not_reserved(relative)?;
        }

        let candidate = if plan_arg.is_absolute() {
            plan_arg.to_path_buf()
        } else {
            root.join(plan_arg)
        };
        if let Ok(canonical) = candidate.canonicalize()
            && let Ok(relative) = canonical.strip_prefix(&root)
        {
            ensure_not_reserved(relative)?;
        }
        Ok(())
    }

    fn ensure_not_reserved(relative: &Path) -> Result<()> {
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => parts.push(part.to_string_lossy()),
                Component::ParentDir => parts.push("..".into()),
                Component::RootDir | Component::Prefix(_) => return Ok(()),
            }
        }
        let folded = parts.join("/").to_ascii_lowercase();
        ensure!(
            folded != EXPORT_STATE_PATH,
            "environment plan cannot target reserved export state `{EXPORT_STATE_PATH}`"
        );
        Ok(())
    }
}
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
pub mod task_cli;
pub mod task_runtime;
pub mod terminal_context;
pub mod tool_profile;
pub mod transaction;
pub mod update;
pub mod vcs;

pub mod tool_versions;
