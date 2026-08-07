use std::time::Duration;

use anyhow::{Context, Result};
use zed_lock::{LockClass, LockManager, LockRequest};

fn main() -> Result<()> {
    let lock_path = std::env::temp_dir()
        .join("zed-lock-example")
        .join("project-mutation.lock");
    let request = LockRequest::exclusive(&lock_path)
        .operation("example project mutation")
        .class(LockClass::ProjectMutation);

    let guard = LockManager::global()
        .acquire_timeout(request, Duration::from_secs(5))
        .with_context(|| format!("acquiring {}", lock_path.display()))?;

    println!(
        "acquired {} for PID {}",
        guard.path().display(),
        guard.owner_info().pid
    );
    guard.release()?;
    Ok(())
}
