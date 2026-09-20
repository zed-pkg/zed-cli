//! Lifecycle-aware public operations facade.
//!
//! The established installer remains in `ops_entry.rs`; this facade brackets
//! root-project operations with convention/configuration lifecycle phases.
//! Dependency-authored install hooks retain their separate consent and
//! allow-list gates in the installer.
//!
//! Repository-owned `contracts/` and `conformance/` are also admitted here so
//! every public package lifecycle uses the same boundary policy. Install/build
//! perform a structural preflight before mutation and execute conformance after
//! success. Test does the same around `zed run test`. Pack/publish execute
//! conformance before producing or publishing an artifact, so a package cannot
//! bypass the boundary by omitting a Git hook.

use std::path::Path;

use anyhow::Result;

use crate::cli::{Adapter, InstallMode};
use crate::config::Config;
use crate::lifecycle::{self, LifecyclePhase};
use crate::project_boundary::{self, BoundaryMode};

#[path = "ops_entry.rs"]
mod core;

pub use core::{
    InstallOutcome, InstallPermissions, WorkspaceInfo, build_publish_meta, cache_clean, find, gc,
    init, login, org_audit, org_claim, split_key, store_prune, store_status, yank,
};

pub(crate) use core::{
    GitLockFinalizeError, detect_adapter, detect_native_manifest_target, detect_structure_target,
    detect_target,
};

#[cfg(test)]
pub(crate) use core::legacy_ensure_artifact_for_test;

fn around_with_boundaries<T>(
    project: &Path,
    pre: LifecyclePhase,
    post: LifecyclePhase,
    pre_mode: BoundaryMode,
    post_mode: BoundaryMode,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    project_boundary::check(project, pre_mode)?;
    let value = lifecycle::around(project, pre, post, operation)?;
    project_boundary::check(project, post_mode)?;
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
pub fn build_cmd(
    project: &Path,
    cfg: &Config,
    force: bool,
    allow_native_deps: bool,
    allow_install_hooks: bool,
    native_manager: Option<&str>,
) -> Result<()> {
    around_with_boundaries(
        project,
        LifecyclePhase::PreBuild,
        LifecyclePhase::PostBuild,
        BoundaryMode::Structural,
        BoundaryMode::Execute,
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

/// `zed run test` is the canonical project-test lifecycle. Arbitrary `zed run`
/// commands remain simple command execution; only the explicit `test` script
/// owns pre-test/post-test hooks and conformance admission.
pub fn run(project: &Path, command: &str, args: &[String]) -> Result<i32> {
    if command != "test" {
        return core::run(project, command, args);
    }

    project_boundary::check(project, BoundaryMode::Structural)?;
    lifecycle::run_phase(project, LifecyclePhase::PreTest)?;
    let code = core::run(project, command, args)?;
    if code != 0 {
        return Ok(code);
    }
    lifecycle::run_phase(project, LifecyclePhase::PostTest)?;
    project_boundary::check(project, BoundaryMode::Execute)?;
    Ok(code)
}

pub fn pack_cmd(project: &Path, out: Option<&Path>) -> Result<Vec<crate::pack::PackagedTarget>> {
    around_with_boundaries(
        project,
        LifecyclePhase::PrePack,
        LifecyclePhase::PostPack,
        BoundaryMode::Execute,
        BoundaryMode::Structural,
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
    around_with_boundaries(
        project,
        LifecyclePhase::PrePublish,
        LifecyclePhase::PostPublish,
        BoundaryMode::Execute,
        BoundaryMode::Structural,
        || core::publish(project, cfg, dry_run, allow_dirty, skip_vcs_checks),
    )
}

pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    around_with_boundaries(
        project,
        LifecyclePhase::PreInstall,
        LifecyclePhase::PostInstall,
        BoundaryMode::Structural,
        BoundaryMode::Execute,
        || core::add(project, cfg, spec),
    )
}

pub fn remove(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    around_with_boundaries(
        project,
        LifecyclePhase::PreUninstall,
        LifecyclePhase::PostUninstall,
        BoundaryMode::Structural,
        BoundaryMode::Structural,
        || core::remove(project, cfg, spec),
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
    around_with_boundaries(
        project,
        LifecyclePhase::PreInstall,
        LifecyclePhase::PostInstall,
        BoundaryMode::Structural,
        BoundaryMode::Execute,
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
    around_with_boundaries(
        project,
        LifecyclePhase::PreInstall,
        LifecyclePhase::PostInstall,
        BoundaryMode::Structural,
        BoundaryMode::Execute,
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
    around_with_boundaries(
        project,
        LifecyclePhase::PreInstall,
        LifecyclePhase::PostInstall,
        BoundaryMode::Structural,
        BoundaryMode::Execute,
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
    around_with_boundaries(
        project,
        LifecyclePhase::PreUninstall,
        LifecyclePhase::PostUninstall,
        BoundaryMode::Structural,
        BoundaryMode::Structural,
        || core::uninstall(project, cfg, specs),
    )
}
