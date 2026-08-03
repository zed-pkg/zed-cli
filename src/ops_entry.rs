//! Public installer facade.
//!
//! The existing implementation continues to own dependency resolution,
//! project transactions, lockfile writes, adapter wiring, and materialization.
//! This facade warms the content-addressed store with the recursive,
//! bounded-concurrency installer before entering that transactional phase.

use std::path::Path;

use anyhow::Result;

use crate::cli::{Adapter, InstallMode};
use crate::config::Config;

#[path = "ops.rs"]
mod implementation;

pub use implementation::{
    InstallOutcome, WorkspaceInfo, add, build_cmd, build_publish_meta, cache_clean, find, gc,
    init, login, org_audit, org_claim, pack_cmd, publish, remove, run, split_key, store_prune,
    store_status, uninstall, yank,
};

pub(crate) use implementation::{
    detect_adapter, detect_native_manifest_target, detect_structure_target, detect_target,
};

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
    crate::install_graph::prefetch(project, cfg, frozen)?;
    implementation::install(
        project,
        cfg,
        frozen,
        mode,
        adapter,
        allow_build,
        target,
        allow_ecosystem_mismatch,
    )
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
    crate::install_graph::prefetch(project, cfg, true)?;
    implementation::install_frozen_lock_only(
        project,
        cfg,
        mode,
        adapter,
        allow_build,
        target,
        allow_ecosystem_mismatch,
    )
}
