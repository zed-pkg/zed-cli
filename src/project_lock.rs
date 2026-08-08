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
use std::rc::{Rc, Weak};

use anyhow::{Context, Result};
use zed_lock::{LockClass, LockGuard, LockManager, LockRequest};

pub const OPERATION_LOCK_RELATIVE_PATH: &str = ".zed/operation.lock";

/// One thread's reentrant ownership cache.
///
/// The map stores weak references so it never extends descriptor ownership.
/// Stale entries are removed lazily on the next acquisition of that canonical
/// checkout. Every live [`OperationGuard`] holds a strong reference to the same
/// [`OwnedLock`], so nested handles may be dropped in any order without
/// releasing the kernel lock before the final handle.
thread_local! {
    static HELD_LOCKS: RefCell<HashMap<PathBuf, Weak<OwnedLock>>> = RefCell::new(HashMap::new());
}

struct OwnedLock {
    _guard: LockGuard,
}

/// Owned checkout-local mutation authority.
///
/// A nested same-thread acquisition clones the strong reference to the outer
/// descriptor ownership. The operating-system lock therefore remains held until
/// the final nested or outer handle is dropped, regardless of drop order.
/// Independent threads and processes always contend through `zed-lock`.
///
/// Reentrancy bookkeeping is thread-local, so this guard is intentionally
/// neither [`Send`] nor [`Sync`]. It must be dropped on the thread that acquired
/// it; moving it would otherwise detach the ownership cache from the originating
/// thread. The `Rc` field enforces both type-level constraints.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<zed_cli::project_lock::OperationGuard>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<zed_cli::project_lock::OperationGuard>();
/// ```
#[must_use = "dropping the operation guard releases one project-ownership handle"]
pub struct OperationGuard {
    _ownership: Rc<OwnedLock>,
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

fn ownership_on_this_thread(path: &Path) -> Option<Rc<OwnedLock>> {
    HELD_LOCKS.with(|held| {
        let mut held = held.borrow_mut();
        let ownership = held.get(path).and_then(Weak::upgrade);
        if ownership.is_none() {
            held.remove(path);
        }
        ownership
    })
}

/// Acquire checkout-local mutation ownership and return an RAII guard.
///
/// This form is useful when callers must span multiple library calls under one
/// project mutation boundary. Nested calls on the same thread share the outer
/// descriptor ownership rather than issuing a second kernel lock request.
pub fn acquire(project: &Path, operation: &str) -> Result<OperationGuard> {
    let path = operation_lock_path(project)?;
    if let Some(ownership) = ownership_on_this_thread(&path) {
        return Ok(OperationGuard {
            _ownership: ownership,
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
    let ownership = Rc::new(OwnedLock { _guard: guard });
    HELD_LOCKS.with(|held| {
        let previous = held
            .borrow_mut()
            .insert(path, Rc::downgrade(&ownership));
        debug_assert!(previous.and_then(|entry| entry.upgrade()).is_none());
    });
    Ok(OperationGuard {
        _ownership: ownership,
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
    use std::rc::Rc;

    use anyhow::Result;
    use zed_lock::{LockClass, LockManager, LockRequest};

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

    #[test]
    fn nested_guards_keep_descriptor_ownership_until_the_last_drop() -> Result<()> {
        let project = tempfile::tempdir()?;
        let path = operation_lock_path(project.path())?;
        let outer = acquire(project.path(), "outer mutation")?;
        let inner = acquire(project.path(), "inner mutation")?;
        assert!(Rc::ptr_eq(&outer._ownership, &inner._ownership));
        assert_eq!(Rc::strong_count(&inner._ownership), 2);

        // Public guards are independent values and callers can drop them out of
        // lexical order. The inner handle must continue owning the descriptor.
        drop(outer);
        assert_eq!(Rc::strong_count(&inner._ownership), 1);
        let contender = LockManager::global().try_acquire(
            LockRequest::exclusive(&path)
                .operation("same-process contention probe")
                .class(LockClass::ProjectMutation)
                .queue_same_process(),
        )?;
        assert!(contender.is_none());

        drop(inner);
        let acquired = LockManager::global().try_acquire(
            LockRequest::exclusive(&path)
                .operation("post-release probe")
                .class(LockClass::ProjectMutation)
                .queue_same_process(),
        )?;
        assert!(acquired.is_some());
        Ok(())
    }
}
