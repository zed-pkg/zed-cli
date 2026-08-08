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
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use zed_lock::{LockClass, LockGuard, LockManager, LockRequest};

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

/// Owned checkout-local mutation authority.
///
/// A nested same-thread acquisition carries only a depth marker because the
/// outer guard already owns the descriptor lock. Independent threads and
/// processes always contend through `zed-lock`.
///
/// Reentrancy bookkeeping is thread-local, so this guard is intentionally
/// neither [`Send`] nor [`Sync`]. It must be dropped on the thread that acquired
/// it; moving it would otherwise detach marker cleanup from the originating
/// thread.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<zed_cli::project_lock::OperationGuard>();
/// ```
#[must_use = "dropping the operation guard releases project mutation ownership"]
pub struct OperationGuard {
    // Drop the thread-local marker before the descriptor lock. That keeps the
    // reentrancy view conservative for the entire lifetime of kernel ownership.
    _marker: HeldMarker,
    _guard: Option<LockGuard>,
    // `Rc` is deliberately !Send + !Sync. The marker itself lives in a
    // thread-local map and therefore cannot be cleaned up safely elsewhere.
    _thread_affinity: PhantomData<Rc<()>>,
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

/// Acquire checkout-local mutation ownership and return an RAII guard.
///
/// This form is useful when callers must span multiple library calls under one
/// project mutation boundary. Nested calls on the same thread reuse the outer
/// descriptor ownership rather than issuing a second kernel lock request.
pub fn acquire(project: &Path, operation: &str) -> Result<OperationGuard> {
    let path = operation_lock_path(project)?;
    if is_held_on_this_thread(&path) {
        return Ok(OperationGuard {
            _marker: HeldMarker::enter(path),
            _guard: None,
            _thread_affinity: PhantomData,
        });
    }

    let guard = LockManager::global()
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
    Ok(OperationGuard {
        _marker: HeldMarker::enter(path),
        _guard: Some(guard),
        _thread_affinity: PhantomData,
    })
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
    let _guard = acquire(project, operation)?;
    action()
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::{OPERATION_LOCK_RELATIVE_PATH, acquire, operation_lock_path, with_lock};

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

    #[test]
    fn explicit_guard_allows_nested_facade_reuse() -> Result<()> {
        let project = tempfile::tempdir()?;
        let _guard = acquire(project.path(), "multi-step mutation")?;
        with_lock(project.path(), "nested facade", || Ok(()))?;
        assert!(operation_lock_path(project.path())?.is_file());
        Ok(())
    }
}
