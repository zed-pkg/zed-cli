use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use zed_cli::lock_waiter::LockWaiter;
use zed_cli::store::{ProcessLock, Store};

const MUST_STILL_BE_BLOCKED: Duration = Duration::from_millis(150);
const ACQUISITION_DEADLINE: Duration = Duration::from_secs(5);

fn install_waiter(home: PathBuf, label: &str) -> Result<LockWaiter<ProcessLock>> {
    LockWaiter::spawn(label, move || Store::new(&home).install_lock())
}

#[test]
fn background_waiter_notifies_only_after_the_kernel_grants_the_lock() -> Result<()> {
    let home = tempfile::tempdir()?;
    let owner = Store::new(home.path()).install_lock()?;
    let mut waiter = install_waiter(home.path().to_path_buf(), "integration-primary")?;

    assert!(
        waiter.wait_timeout(MUST_STILL_BE_BLOCKED)?.is_none(),
        "the waiter reported acquisition while the owner still held install.lock"
    );

    let responsive_marker = home.path().join("main-thread-remained-responsive");
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

    // The guard is transferred through the channel, not released by the
    // waiter thread before notification reaches the caller.
    let mut follower = install_waiter(home.path().to_path_buf(), "integration-follower")?;
    assert!(
        follower
            .wait_timeout(MUST_STILL_BE_BLOCKED)?
            .is_none(),
        "transferring the guard to the caller released it prematurely"
    );
    drop(acquired);
    let follower_guard = follower
        .wait_timeout(ACQUISITION_DEADLINE)?
        .context("the follower was not notified after the transferred guard dropped")?;
    drop(follower_guard);
    Ok(())
}

#[test]
fn multiple_waiters_receive_exclusive_handoffs_without_assuming_fifo_order() -> Result<()> {
    const WAITER_COUNT: usize = 4;

    let home = tempfile::tempdir()?;
    let owner = Store::new(home.path()).install_lock()?;
    let start = Arc::new(Barrier::new(WAITER_COUNT + 1));
    let (acquired_sender, acquired_receiver) = mpsc::channel::<(usize, ProcessLock)>();
    let mut workers = Vec::with_capacity(WAITER_COUNT);

    for waiter_id in 0..WAITER_COUNT {
        let home = home.path().to_path_buf();
        let start = Arc::clone(&start);
        let acquired_sender = acquired_sender.clone();
        workers.push(thread::spawn(move || -> Result<()> {
            start.wait();
            let guard = Store::new(&home).install_lock()?;
            acquired_sender
                .send((waiter_id, guard))
                .map_err(|_| anyhow!("acquisition receiver closed"))?;
            Ok(())
        }));
    }
    drop(acquired_sender);

    start.wait();
    assert!(
        matches!(
            acquired_receiver.recv_timeout(MUST_STILL_BE_BLOCKED),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "a waiter acquired install.lock before the original owner released it"
    );

    drop(owner);
    let mut seen = BTreeSet::new();
    for handoff in 0..WAITER_COUNT {
        let (waiter_id, guard) = acquired_receiver
            .recv_timeout(ACQUISITION_DEADLINE)
            .context("a blocked waiter was not awakened after lock release")?;
        assert!(seen.insert(waiter_id), "waiter {waiter_id} notified twice");

        if handoff + 1 < WAITER_COUNT {
            assert!(
                matches!(
                    acquired_receiver.recv_timeout(MUST_STILL_BE_BLOCKED),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ),
                "more than one waiter held the exclusive lock at the same time"
            );
        }
        drop(guard);
    }

    for worker in workers {
        worker
            .join()
            .map_err(|_| anyhow!("lock waiter worker panicked"))??;
    }
    assert_eq!(seen.len(), WAITER_COUNT);
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
fn unrelated_lock_files_do_not_serialize_evented_waiters() -> Result<()> {
    let first_home = tempfile::tempdir()?;
    let second_home = tempfile::tempdir()?;
    let first_owner = Store::new(first_home.path()).install_lock()?;

    let mut second_waiter =
        install_waiter(second_home.path().to_path_buf(), "integration-independent")?;
    let second_guard = second_waiter
        .wait_timeout(ACQUISITION_DEADLINE)?
        .context("an unrelated lock file was incorrectly serialized")?;

    drop(second_guard);
    drop(first_owner);
    Ok(())
}

#[cfg(unix)]
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
