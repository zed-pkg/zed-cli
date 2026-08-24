//! Lifecycle-aware public operations facade.
//!
//! The established installer remains in `ops_entry.rs`; this facade brackets
//! root-project operations with convention/configuration lifecycle phases.

use std::path::Path;

use anyhow::Result;

use crate::cli::{Adapter, InstallMode};
use crate::config::Config;
use crate::lifecycle::{self, LifecyclePhase};

#[path = "ops_entry.rs"]
mod core;

pub use core::{
    InstallOutcome, InstallPermissions, WorkspaceInfo, add, build_publish_meta, cache_clean, find,
    gc, init, login, org_audit, org_claim, remove, run, split_key, store_prune, store_status, yank,
};

pub(crate) use core::{
    detect_adapter, detect_native_manifest_target, detect_structure_target, detect_target,
};

#[cfg(test)]
pub(crate) use core::legacy_ensure_artifact_for_test;

#[allow(clippy::too_many_arguments)]
pub fn build_cmd(
    project: &Path,
    cfg: &Config,
    force: bool,
    allow_native_deps: bool,
    allow_install_hooks: bool,
    native_manager: Option<&str>,
) -> Result<()> {
    lifecycle::around(
        project,
        LifecyclePhase::PreBuild,
        LifecyclePhase::PostBuild,
        || {
            core::build_cmd(
                project,
                cfg,
                force,
                allow_native_deps,
                allow_install_hooks,
                native_manager,
            )
        },
    )
}

pub fn pack_cmd(project: &Path, out: Option<&Path>) -> Result<Vec<crate::pack::PackagedTarget>> {
    lifecycle::around(
        project,
        LifecyclePhase::PrePack,
        LifecyclePhase::PostPack,
        || core::pack_cmd(project, out),
    )
}

pub fn publish(
    project: &Path,
    cfg: &Config,
    dry_run: bool,
    allow_dirty: bool,
    skip_vcs_checks: bool,
) -> Result<()> {
    lifecycle::around(
        project,
        LifecyclePhase::PrePublish,
        LifecyclePhase::PostPublish,
        || core::publish(project, cfg, dry_run, allow_dirty, skip_vcs_checks),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    lifecycle::around(
        project,
        LifecyclePhase::PreInstall,
        LifecyclePhase::PostInstall,
        || {
            core::install(
                project,
                cfg,
                frozen,
                mode,
                adapter,
                allow_build,
                target,
                allow_ecosystem_mismatch,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn install_with_permissions(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &InstallPermissions,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    lifecycle::around(
        project,
        LifecyclePhase::PreInstall,
        LifecyclePhase::PostInstall,
        || {
            core::install_with_permissions(
                project,
                cfg,
                frozen,
                mode,
                adapter,
                permissions,
                target,
                allow_ecosystem_mismatch,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_frozen_lock_only_with_permissions(
    project: &Path,
    cfg: &Config,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &InstallPermissions,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    lifecycle::around(
        project,
        LifecyclePhase::PreInstall,
        LifecyclePhase::PostInstall,
        || {
            core::install_frozen_lock_only_with_permissions(
                project,
                cfg,
                mode,
                adapter,
                permissions,
                target,
                allow_ecosystem_mismatch,
            )
        },
    )
}

pub fn uninstall(project: &Path, cfg: &Config, specs: &[String]) -> Result<()> {
    lifecycle::around(
        project,
        LifecyclePhase::PreUninstall,
        LifecyclePhase::PostUninstall,
        || core::uninstall(project, cfg, specs),
    )
}
