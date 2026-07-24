//! Build cache (zed-docs issue #5).
//!
//! Source is universal; compiled output is not. zed-pkg keeps the two in
//! separate caches: the content-addressed **source store** stays
//! platform-independent and shareable, while build output lands in a
//! **build cache** keyed by `(target triple, source sha256, build command)`.
//! A package with a `[build]` step is never compiled inside the immutable
//! source store — it is copied into a sandbox, built there, and the result is
//! promoted into the build cache and linked into the project.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::manifest::BuildSection;
use zed_interfaces::paths::{MODULES_DIR, STORE_PKG_DIR};

use crate::store::Store;

/// The build target triple used to key the build cache. `{arch}-{os}` keeps
/// e.g. `linux-x86_64` and `macos-aarch64` builds from ever colliding.
pub fn target_triple() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The cache key for a source artifact built with a given command: the source
/// sha256 plus a short hash of the command and declared outputs. Two builds
/// with different commands (e.g. a consumer override) never share an entry.
fn cache_key(source_sha: &str, command: &str, outputs: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(command.as_bytes());
    hasher.update([0]);
    for out in outputs {
        hasher.update(out.as_bytes());
        hasher.update([0]);
    }
    let cmd_hash = hex::encode(hasher.finalize());
    format!("{source_sha}-{}", &cmd_hash[..16])
}

/// Ensure the build step has produced output for `source_dir` (whose artifact
/// hash is `source_sha`) on the current target, returning the directory of
/// built files. A cache hit returns immediately; a miss builds in a sandbox
/// copy of the source and promotes the result. `prepare_sandbox` runs in the
/// work tree before the command — used to link build-dependencies in.
pub fn ensure_built(
    store: &Store,
    triple: &str,
    source_sha: &str,
    source_dir: &Path,
    build: &BuildSection,
    force: bool,
    prepare_sandbox: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    let command = build
        .command
        .as_deref()
        .context("build step has no command")?;
    let key = cache_key(source_sha, command, &build.outputs);

    if !force && store.has_build(triple, &key) {
        return Ok(store.build_pkg_dir(triple, &key));
    }
    let _lock = store.build_lock(&key)?;
    if !force && store.has_build(triple, &key) {
        return Ok(store.build_pkg_dir(triple, &key));
    }

    let entry = store.build_entry(triple, &key);
    let parent = entry.parent().context("build entry has a parent")?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let work = staging.path().join("work");
    copy_tree(source_dir, &work)?;
    prepare_sandbox(&work)?;

    // Build-dependencies are exposed under the work tree; drop them before
    // promotion so they never leak into the built artifact.
    let build_modules = work.join(MODULES_DIR);

    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&work)
        .env("ZED_PKG_TARGET", triple)
        .env("ZED_PKG_MODULES", &build_modules)
        .env("NODE_PATH", &build_modules)
        .status()
        .with_context(|| format!("running build command `{command}`"))?;
    if !status.success() {
        bail!("build command `{command}` failed with {status}");
    }
    if build_modules.exists() {
        let _ = std::fs::remove_dir_all(&build_modules);
    }

    let out = staging.path().join(STORE_PKG_DIR);
    if build.outputs.is_empty() {
        std::fs::rename(&work, &out)?;
    } else {
        std::fs::create_dir_all(&out)?;
        for rel in &build.outputs {
            let src = work.join(rel);
            if !src.exists() {
                bail!("build output `{rel}` was not produced by `{command}`");
            }
            let dst = out.join(rel);
            std::fs::create_dir_all(dst.parent().context("output has a parent")?)?;
            if src.is_dir() {
                copy_tree(&src, &dst)?;
            } else {
                std::fs::copy(&src, &dst)?;
            }
        }
    }

    std::fs::create_dir_all(&entry)?;
    match std::fs::rename(&out, entry.join(STORE_PKG_DIR)) {
        Ok(()) => {}
        // Lost a race with a concurrent build; theirs is fine.
        Err(_) if store.has_build(triple, &key) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(store.build_pkg_dir(triple, &key))
}

/// Recursively copy a directory tree, materializing files (never symlinks) so
/// the build sandbox and promoted artifact are self-contained.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
