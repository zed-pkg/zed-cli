from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"expected one match in {path}, found {count}")
    file.write_text(text.replace(old, new, 1))


replace_once(
    "src/install_graph/tests.rs",
    '''use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
    mpsc as test_mpsc,
};
use std::thread;
use std::time::Duration;
''',
    '''use std::process::{Command, Stdio};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
    mpsc as test_mpsc,
};
use std::thread;
use std::time::{Duration, Instant};
''',
)

replace_once(
    "src/install_graph/tests.rs",
    '''#[cfg(unix)]
#[test]
fn artifact_waiters_block_until_the_owner_releases_the_kernel_lock() {
    let temp = tempfile::tempdir().unwrap();
    let sha = "a".repeat(64);
    let owner = ArtifactProcessLock::acquire(temp.path(), &sha).unwrap();
    let second_home = temp.path().to_path_buf();
    let second_sha = sha.clone();
    let (acquired_tx, acquired_rx) = test_mpsc::channel();
    let waiter = thread::spawn(move || {
        let _waiter = ArtifactProcessLock::acquire(&second_home, &second_sha).unwrap();
        acquired_tx.send(()).unwrap();
    });

    assert!(
        acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    drop(owner);
    acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    waiter.join().unwrap();
}
''',
    '''#[test]
fn artifact_waiters_block_until_the_owner_releases_the_kernel_lock() {
    const CHILD_HOME: &str = "ZED_PKG_TEST_LOCK_CHILD_HOME";
    const CHILD_SHA: &str = "ZED_PKG_TEST_LOCK_CHILD_SHA";
    const CHILD_ATTEMPTING: &str = "ZED_PKG_TEST_LOCK_CHILD_ATTEMPTING";
    const CHILD_ACQUIRED: &str = "ZED_PKG_TEST_LOCK_CHILD_ACQUIRED";
    const TEST_NAME: &str =
        "install_graph::tests::artifact_waiters_block_until_the_owner_releases_the_kernel_lock";

    if let Some(home) = std::env::var_os(CHILD_HOME) {
        let sha = std::env::var(CHILD_SHA).unwrap();
        let attempting = PathBuf::from(std::env::var_os(CHILD_ATTEMPTING).unwrap());
        let acquired = PathBuf::from(std::env::var_os(CHILD_ACQUIRED).unwrap());
        fs::write(attempting, b"attempting").unwrap();
        let _waiter = ArtifactProcessLock::acquire(Path::new(&home), &sha).unwrap();
        fs::write(acquired, b"acquired").unwrap();
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let sha = "a".repeat(64);
    let owner = ArtifactProcessLock::acquire(temp.path(), &sha).unwrap();
    let attempting = temp.path().join("waiter-attempting");
    let acquired = temp.path().join("waiter-acquired");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg(TEST_NAME)
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_HOME, temp.path())
        .env(CHILD_SHA, &sha)
        .env(CHILD_ATTEMPTING, &attempting)
        .env(CHILD_ACQUIRED, &acquired)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let attempting_deadline = Instant::now() + Duration::from_secs(5);
    while !attempting.is_file() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("lock waiter child exited before attempting acquisition: {status}");
        }
        if Instant::now() >= attempting_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lock waiter child did not reach acquisition");
        }
        thread::sleep(Duration::from_millis(20));
    }

    thread::sleep(Duration::from_millis(200));
    assert!(
        !acquired.exists(),
        "a separate process acquired the artifact lock while the owner still held it"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "lock waiter child exited before the owner released the lock"
    );

    drop(owner);
    let release_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "lock waiter child failed: {status}");
            break;
        }
        if Instant::now() >= release_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lock waiter child did not wake after lock release");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        acquired.is_file(),
        "lock waiter child exited without recording acquisition"
    );
}
''',
)
