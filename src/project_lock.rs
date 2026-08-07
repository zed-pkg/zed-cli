//! Checkout-local serialization for project-tree mutations.
//!
//! The global content-addressed store has its own fine-grained locks. This
//! module protects mutable state owned by one checkout: manifests, lockfiles,
//! generated adapter wiring, Git-submodule projections, and transaction
//! recovery. The lock lives inside the canonical project directory so two Zed
//! processes using different `ZED_PKG_HOME` values still coordinate.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zed_lock::{LockClass, LockManager, LockRequest};

pub const OPERATION_LOCK_RELATIVE_PATH: &str = ".zed/operation.lock";

thread_local! {
    /// Same-thread facade composition is intentionally reentrant. For example,
    /// the CLI may hold the checkout lock around Git-submodule synchronization
    /// and then call the public installer facade, which must reuse that exact
    /// ownership rather than deadlock on a second descriptor request.
    static HELD_LOCKS: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());
}

struct HeldMarker {
    path: PathBuf,
}

impl HeldMarker {
    fn enter(path: PathBuf) -> Self {
        HELD_LOCKS.with(|held| {
            *held.borrow_mut().entry(path.clone()).or_insert(0) += 1;
        });
        Self { path }
    }
}

impl Drop for HeldMarker {
    fn drop(&mut self) {
        HELD_LOCKS.with(|held| {
            let mut held = held.borrow_mut();
            let remove = match held.get_mut(&self.path) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                    false
                }
                Some(_) => true,
                None => false,
            };
            if remove {
                held.remove(&self.path);
            }
        });
    }
}

/// Canonical descriptor-lock identity for one checkout.
///
/// Canonicalizing the project before appending the stable relative path makes
/// symlink aliases converge on the same lock file. The lock file is a durable
/// rendezvous point; its diagnostic contents are never ownership authority.
pub fn operation_lock_path(project: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing project root {}", project.display()))?;
    Ok(canonical.join(OPERATION_LOCK_RELATIVE_PATH))
}

fn is_held_on_this_thread(path: &Path) -> bool {
    HELD_LOCKS.with(|held| held.borrow().contains_key(path))
}

/// Run a complete project mutation while owning the checkout-local lock.
///
/// Nested calls on the same thread and canonical checkout reuse the outer
/// ownership. Independent threads and processes still go through `zed-lock`,
/// whose descriptor lock is the sole local ownership authority and is released
/// automatically if the process exits.
pub fn with_lock<T>(
    project: &Path,
    operation: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let path = operation_lock_path(project)?;
    if is_held_on_this_thread(&path) {
        let _marker = HeldMarker::enter(path);
        return action();
    }

    let _guard = LockManager::global()
        .acquire_blocking(
            LockRequest::exclusive(&path)
                .operation(operation)
                .class(LockClass::ProjectMutation),
        )
        .with_context(|| {
            format!(
                "acquiring project operation lock for `{operation}` at {}",
                path.display()
            )
        })?;
    let _marker = HeldMarker::enter(path);
    action()
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::{OPERATION_LOCK_RELATIVE_PATH, operation_lock_path, with_lock};

    #[test]
    fn canonical_aliases_share_one_lock_identity() -> Result<()> {
        let project = tempfile::tempdir()?;
        let direct = operation_lock_path(project.path())?;
        assert!(direct.ends_with(OPERATION_LOCK_RELATIVE_PATH));

        #[cfg(unix)]
        {
            let alias_root = tempfile::tempdir()?;
            let alias = alias_root.path().join("checkout");
            std::os::unix::fs::symlink(project.path(), &alias)?;
            assert_eq!(operation_lock_path(&alias)?, direct);
        }
        Ok(())
    }

    #[test]
    fn same_thread_facades_reuse_outer_ownership() -> Result<()> {
        let project = tempfile::tempdir()?;
        with_lock(project.path(), "outer mutation", || {
            with_lock(project.path(), "nested mutation", || Ok(()))
        })?;
        assert!(operation_lock_path(project.path())?.is_file());
        Ok(())
    }
}
