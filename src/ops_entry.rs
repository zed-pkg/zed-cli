//! Public installer facade.
//!
//! Graph solving and artifact acquisition happen before the implementation's
//! project transaction. Non-frozen installs expose the solver's exact registry
//! selections only to the root consumer manifest, so the established installer
//! writes the lockfile, adapters, and materialization from that one graph rather
//! than independently making greedy choices. Frozen replay remains lock-driven.
//!
//! Adopted Git submodules are verified before mutation and recorded through an
//! additive lock extension after the ordinary install transaction. Older lock
//! readers ignore that extension; this facade keeps it exact across install,
//! add, remove, and frozen replay.
//!
//! Dart's provisional adapter fragment is finalized here after materialization,
//! replacing Zed directory-derived keys with the native package identities from
//! each dependency's `pubspec.yaml`. The hook is a no-op for every other
//! adapter and is shared by normal and manifestless frozen installs.

use std::path::Path;

use anyhow::{Context, Result};

use crate::cli::{Adapter, InstallMode};
use crate::config::{self, Config};

#[derive(Debug)]
pub(crate) struct GitLockFinalizeError;

impl std::fmt::Display for GitLockFinalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("finalizing adopted Git submodule lock metadata")
    }
}

impl std::error::Error for GitLockFinalizeError {}

#[path = "ops.rs"]
mod implementation;

pub use implementation::{
    InstallOutcome, WorkspaceInfo, build_cmd, build_publish_meta, cache_clean, find, gc, init,
    login, org_audit, org_claim, run, split_key, store_prune, store_status, uninstall, yank,
};

pub(crate) use implementation::{
    detect_adapter, detect_native_manifest_target, detect_structure_target, detect_target,
};

#[cfg(test)]
pub(crate) use implementation::legacy_ensure_artifact_for_test;

fn with_pack_guard<T>(project: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let manifest = config::read_manifest(project)?;
    let manifest = crate::pack_guard::harden_manifest(manifest);
    crate::pack_guard::preflight_submodules(project, &manifest)?;
    let manifest_text = manifest.to_toml_string()?;
    config::with_manifest_override(project, manifest_text, action)
}

pub fn pack_cmd(project: &Path, out: Option<&Path>) -> Result<Vec<crate::pack::PackagedTarget>> {
    with_pack_guard(project, || implementation::pack_cmd(project, out))
}

pub fn publish(
    project: &Path,
    cfg: &Config,
    dry_run: bool,
    allow_dirty: bool,
    skip_vcs_checks: bool,
) -> Result<()> {
    with_pack_guard(project, || {
        implementation::publish(project, cfg, dry_run, allow_dirty, skip_vcs_checks)
    })
}

pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    crate::git_submodules::preflight_mutation(project)?;
    crate::config::with_install_prefetch(cfg, || implementation::add(project, cfg, spec))?;
    crate::git_submodules::refresh_lock_extensions(project)
}

pub fn remove(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    crate::git_submodules::preflight_mutation(project)?;
    crate::config::with_install_prefetch(cfg, || implementation::remove(project, cfg, spec))?;
    crate::git_submodules::refresh_lock_extensions(project)
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
    let git_lock = crate::git_submodules::prepare_install(project, frozen)?;
    let outcome = if frozen {
        crate::install_graph::prefetch(project, cfg, true)?;
        implementation::install(
            project,
            cfg,
            true,
            mode,
            adapter,
            allow_build,
            target,
            allow_ecosystem_mismatch,
        )?
    } else {
        let prepared = crate::install_graph::prepare(project, cfg)?;
        config::with_resolved_requirements(project, prepared.exact_requirements(), || {
            implementation::install(
                project,
                cfg,
                false,
                mode,
                adapter,
                allow_build,
                target,
                allow_ecosystem_mismatch,
            )
        })?
    };
    crate::dart_wiring::rewrite_if_present(project)
        .context("finalizing Dart package-manager wiring")?;
    git_lock.finish(project).context(GitLockFinalizeError)?;
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_frozen_lock_only(
    project: &Path,
    cfg: &Config,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    let git_lock = crate::git_submodules::prepare_install(project, true)?;
    crate::install_graph::prefetch(project, cfg, true)?;
    let outcome = implementation::install_frozen_lock_only(
        project,
        cfg,
        mode,
        adapter,
        allow_build,
        target,
        allow_ecosystem_mismatch,
    )?;
    crate::dart_wiring::rewrite_if_present(project)
        .context("finalizing Dart package-manager wiring")?;
    git_lock.finish(project).context(GitLockFinalizeError)?;
    Ok(outcome)
}
