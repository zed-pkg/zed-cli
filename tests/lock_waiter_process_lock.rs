#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use zed_cli::lock_waiter::LockWaiter;
use zed_cli::store::{ProcessLock, Store};

const MUST_STILL_BE_BLOCKED: Duration = Duration::from_millis(150);
const ACQUISITION_DEADLINE: Duration = Duration::from_secs(5);

fn install_waiter(home: PathBuf, label: &str) -> Result<LockWaiter<ProcessLock>> {
    LockWaiter::spawn(label, move || Store::new(&home).install_lock())
}

#[test]
fn background_waiter_reports_only_after_the_kernel_grants_and_transfers_the_lock() -> Result<()> {
    let home = tempfile::tempdir()?;
    let owner = Store::new(home.path()).install_lock()?;
    let mut waiter = install_waiter(home.path().to_path_buf(), "integration-primary")?;

    assert!(
        waiter.wait_timeout(MUST_STILL_BE_BLOCKED)?.is_none(),
        "the waiter reported acquisition while the owner still held install.lock"
    );

    let responsive_marker = home.path().join("caller-remained-responsive");
    fs::write(&responsive_marker, b"completed unrelated work")?;

    drop(owner);
    let acquired = waiter
        .wait_timeout(ACQUISITION_DEADLINE)?
        .context("the waiter was not notified after orderly owner release")?;
    assert_eq!(
        fs::read(&responsive_marker)?,
        b"completed unrelated work",
        "the caller should be able to work while the waiter thread sleeps"
    );

    let mut follower = install_waiter(home.path().to_path_buf(), "integration-follower")?;
    assert!(
        follower.wait_timeout(MUST_STILL_BE_BLOCKED)?.is_none(),
        "transferring the guard through the channel released it prematurely"
    );
    drop(acquired);
    let follower_guard = follower
        .wait_timeout(ACQUISITION_DEADLINE)?
        .context("the follower was not notified after the transferred guard dropped")?;
    drop(follower_guard);
    Ok(())
}

#[test]
fn panic_drops_the_descriptor_and_wakes_the_background_waiter() -> Result<()> {
    let home = tempfile::tempdir()?;
    let owner_home = home.path().to_path_buf();
    let (owner_ready_sender, owner_ready_receiver) = mpsc::sync_channel(0);
    let (panic_sender, panic_receiver) = mpsc::sync_channel(0);

    let owner = thread::spawn(move || {
        let _guard = Store::new(&owner_home).install_lock().unwrap();
        owner_ready_sender.send(()).unwrap();
        panic_receiver.recv().unwrap();
        panic!("intentional owner panic");
    });

    owner_ready_receiver
        .recv_timeout(ACQUISITION_DEADLINE)
        .context("owner thread did not acquire install.lock")?;
    let mut waiter = install_waiter(home.path().to_path_buf(), "integration-panic")?;
    assert!(
        waiter.wait_timeout(MUST_STILL_BE_BLOCKED)?.is_none(),
        "the waiter acquired install.lock while the owner thread was alive"
    );

    panic_sender.send(()).unwrap();
    assert!(owner.join().is_err(), "owner thread should have panicked");
    let guard = waiter
        .wait_timeout(ACQUISITION_DEADLINE)?
        .context("the waiter was not awakened after panic dropped the descriptor")?;
    drop(guard);
    Ok(())
}

#[test]
fn symlink_aliases_contend_on_the_same_kernel_lock_inode() -> Result<()> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir()?;
    let real_home = root.path().join("real-home");
    let alias_home = root.path().join("alias-home");
    fs::create_dir_all(&real_home)?;
    symlink(&real_home, &alias_home)?;

    let owner = Store::new(&real_home).install_lock()?;
    let mut waiter = install_waiter(alias_home, "integration-symlink-alias")?;
    assert!(
        waiter.wait_timeout(MUST_STILL_BE_BLOCKED)?.is_none(),
        "a symlink alias bypassed the descriptor lock"
    );

    drop(owner);
    let guard = waiter
        .wait_timeout(ACQUISITION_DEADLINE)?
        .context("the alias waiter was not awakened after owner release")?;
    drop(guard);
    Ok(())
}
