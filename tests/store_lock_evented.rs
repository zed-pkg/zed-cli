use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zed_cli::store::Store;

const HELPER_ROLE: &str = "ZED_PKG_TEST_LOCK_HELPER_ROLE";
const HELPER_HOME: &str = "ZED_PKG_TEST_LOCK_HELPER_HOME";
const HELPER_ATTEMPTING: &str = "ZED_PKG_TEST_LOCK_HELPER_ATTEMPTING";
const HELPER_ACQUIRED: &str = "ZED_PKG_TEST_LOCK_HELPER_ACQUIRED";
const HELPER_HOLD_MS: &str = "ZED_PKG_TEST_LOCK_HELPER_HOLD_MS";
const HELPER_BUILD_KEY: &str = "ZED_PKG_TEST_LOCK_HELPER_BUILD_KEY";
const HELPER_CRITICAL_SECTION: &str = "ZED_PKG_TEST_LOCK_HELPER_CRITICAL_SECTION";
const HELPER_OVERLAP_MARKER: &str = "ZED_PKG_TEST_LOCK_HELPER_OVERLAP_MARKER";
const HELPER_TEST: &str = "process_lock_helper";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

struct ManagedChild {
    child: Option<Child>,
    label: String,
}

impl ManagedChild {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        home: &Path,
        role: &str,
        attempting: &Path,
        acquired: &Path,
        hold: Duration,
        build_key: Option<&str>,
        critical_section: Option<&Path>,
        overlap_marker: Option<&Path>,
        label: impl Into<String>,
    ) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg(HELPER_TEST)
            .arg("--exact")
            .arg("--nocapture")
            .env(HELPER_ROLE, role)
            .env(HELPER_HOME, home)
            .env(HELPER_ATTEMPTING, attempting)
            .env(HELPER_ACQUIRED, acquired)
            .env(HELPER_HOLD_MS, hold.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(build_key) = build_key {
            command.env(HELPER_BUILD_KEY, build_key);
        }
        if let Some(critical_section) = critical_section {
            command.env(HELPER_CRITICAL_SECTION, critical_section);
        }
        if let Some(overlap_marker) = overlap_marker {
            command.env(HELPER_OVERLAP_MARKER, overlap_marker);
        }
        let label = label.into();
        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawning {label}: {error}"));
        Self {
            child: Some(child),
            label,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("managed child still present")
    }

    fn wait_for_marker(&mut self, path: &Path) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !path.is_file() {
            if let Some(status) = self.child_mut().try_wait().expect("checking child status") {
                panic!(
                    "{} exited before writing {}: {status}",
                    self.label,
                    path.display()
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "{} did not write {} before the timeout",
                    self.label,
                    path.display()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_running(&mut self) {
        let status = self.child_mut().try_wait().expect("checking child status");
        assert!(
            status.is_none(),
            "{} exited unexpectedly: {status:?}",
            self.label
        );
    }

    fn wait_success(&mut self) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Some(status) = self.child_mut().try_wait().expect("checking child status") {
                self.child.take();
                assert!(status.success(), "{} failed: {status}", self.label);
                return;
            }
            if Instant::now() >= deadline {
                let mut child = self.child.take().expect("managed child still present");
                let _ = child.kill();
                let status = child.wait().expect("reaping timed-out child");
                panic!("{} timed out; final status: {status}", self.label);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill_and_wait(&mut self) -> ExitStatus {
        let mut child = self.child.take().expect("managed child still present");
        child.kill().expect("terminating lock owner");
        child.wait().expect("reaping terminated lock owner")
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn process_lock_helper() {
    let Some(role) = std::env::var_os(HELPER_ROLE) else {
        return;
    };
    let role = role.to_string_lossy().into_owned();
    let home = PathBuf::from(std::env::var_os(HELPER_HOME).expect("helper home"));
    let attempting = PathBuf::from(std::env::var_os(HELPER_ATTEMPTING).expect("attempting marker"));
    let acquired = PathBuf::from(std::env::var_os(HELPER_ACQUIRED).expect("acquired marker"));
    let hold_ms = std::env::var(HELPER_HOLD_MS)
        .expect("helper hold duration")
        .parse::<u64>()
        .expect("numeric helper hold duration");

    fs::write(&attempting, b"attempting").expect("writing attempting marker");
    let store = Store::new(&home);
    let guard = match role.as_str() {
        "install" => store.install_lock().expect("acquiring install lock"),
        "build" => {
            let key = std::env::var(HELPER_BUILD_KEY).expect("build-lock key");
            store
                .build_lock("test-platform", &key)
                .expect("acquiring build lock")
        }
        other => panic!("unknown helper role: {other}"),
    };

    let critical_section = std::env::var_os(HELPER_CRITICAL_SECTION).map(PathBuf::from);
    if let Some(path) = critical_section.as_ref()
        && let Err(error) = fs::create_dir(path)
    {
        if let Some(marker) = std::env::var_os(HELPER_OVERLAP_MARKER) {
            let _ = fs::write(
                marker,
                format!("{} could not enter {}: {error}\n", role, path.display()),
            );
        }
        panic!(
            "lock-protected critical section overlapped at {}: {error}",
            path.display()
        );
    }

    fs::write(&acquired, b"acquired").expect("writing acquired marker");
    thread::sleep(Duration::from_millis(hold_ms));

    if let Some(path) = critical_section {
        fs::remove_dir(&path).unwrap_or_else(|error| {
            panic!(
                "removing critical-section marker {}: {error}",
                path.display()
            )
        });
    }
    drop(guard);
}

#[test]
fn contended_install_waiters_wake_and_serialize_after_release() {
    let temp = tempfile::tempdir().expect("temporary lock home");
    let store = Store::new(temp.path());
    let owner = store.install_lock().expect("owner install lock");
    let critical_section = temp.path().join("install-critical-section");
    let overlap_marker = temp.path().join("install-critical-section-overlap");

    let markers = (0..4)
        .map(|index| {
            (
                temp.path().join(format!("waiter-{index}-attempting")),
                temp.path().join(format!("waiter-{index}-acquired")),
            )
        })
        .collect::<Vec<_>>();
    let mut waiters = markers
        .iter()
        .enumerate()
        .map(|(index, (attempting, acquired))| {
            ManagedChild::spawn(
                temp.path(),
                "install",
                attempting,
                acquired,
                Duration::from_millis(75),
                None,
                Some(&critical_section),
                Some(&overlap_marker),
                format!("install waiter {index}"),
            )
        })
        .collect::<Vec<_>>();

    for (waiter, (attempting, _)) in waiters.iter_mut().zip(&markers) {
        waiter.wait_for_marker(attempting);
    }
    thread::sleep(Duration::from_millis(150));
    for (waiter, (_, acquired)) in waiters.iter_mut().zip(&markers) {
        assert!(
            !acquired.exists(),
            "a waiter entered while the original owner still held install.lock"
        );
        waiter.assert_running();
    }

    drop(owner);
    for waiter in &mut waiters {
        waiter.wait_success();
    }

    assert!(
        markers.iter().all(|(_, acquired)| acquired.is_file()),
        "every queued waiter must eventually acquire after successive releases"
    );
    assert!(
        !overlap_marker.exists(),
        "two install-lock owners entered the critical section together"
    );
    assert!(
        !critical_section.exists(),
        "the final owner left the critical-section marker behind"
    );
}

#[test]
fn releasing_an_unrelated_build_lock_does_not_wake_an_install_waiter() {
    let temp = tempfile::tempdir().expect("temporary lock home");
    let store = Store::new(temp.path());
    let install_owner = store.install_lock().expect("owner install lock");
    let build_key = "a".repeat(64);
    let unrelated_build = store
        .build_lock("test-platform", &build_key)
        .expect("unrelated build lock");
    let attempting = temp.path().join("install-waiter-attempting");
    let acquired = temp.path().join("install-waiter-acquired");
    let mut waiter = ManagedChild::spawn(
        temp.path(),
        "install",
        &attempting,
        &acquired,
        Duration::ZERO,
        None,
        None,
        None,
        "install waiter",
    );

    waiter.wait_for_marker(&attempting);
    thread::sleep(Duration::from_millis(100));
    assert!(!acquired.exists());

    drop(unrelated_build);
    thread::sleep(Duration::from_millis(150));
    assert!(
        !acquired.exists(),
        "releasing a different lock key woke the install waiter"
    );
    waiter.assert_running();

    drop(install_owner);
    waiter.wait_success();
    assert!(acquired.is_file());
}

#[test]
fn independent_store_homes_do_not_share_an_install_lock() {
    let temp = tempfile::tempdir().expect("temporary roots");
    let home_a = temp.path().join("home-a");
    let home_b = temp.path().join("home-b");
    let store_a = Store::new(&home_a);
    let owner_a = store_a.install_lock().expect("home-a install lock");
    let attempting = temp.path().join("home-b-attempting");
    let acquired = temp.path().join("home-b-acquired");
    let mut home_b_waiter = ManagedChild::spawn(
        &home_b,
        "install",
        &attempting,
        &acquired,
        Duration::ZERO,
        None,
        None,
        None,
        "home-b install owner",
    );

    home_b_waiter.wait_for_marker(&attempting);
    home_b_waiter.wait_success();
    assert!(
        acquired.is_file(),
        "an install lock in a different home was incorrectly serialized"
    );
    drop(owner_a);
}

#[test]
fn same_build_key_serializes_while_a_distinct_key_progresses() {
    let temp = tempfile::tempdir().expect("temporary lock home");
    let store = Store::new(temp.path());
    let key_a = "a".repeat(64);
    let key_b = "b".repeat(64);
    let owner_a = store
        .build_lock("test-platform", &key_a)
        .expect("build-key-a owner");

    let same_attempting = temp.path().join("same-key-attempting");
    let same_acquired = temp.path().join("same-key-acquired");
    let different_attempting = temp.path().join("different-key-attempting");
    let different_acquired = temp.path().join("different-key-acquired");
    let mut same_key = ManagedChild::spawn(
        temp.path(),
        "build",
        &same_attempting,
        &same_acquired,
        Duration::ZERO,
        Some(&key_a),
        None,
        None,
        "same build-key waiter",
    );
    let mut different_key = ManagedChild::spawn(
        temp.path(),
        "build",
        &different_attempting,
        &different_acquired,
        Duration::ZERO,
        Some(&key_b),
        None,
        None,
        "different build-key owner",
    );

    same_key.wait_for_marker(&same_attempting);
    different_key.wait_for_marker(&different_attempting);
    different_key.wait_success();
    assert!(different_acquired.is_file());
    assert!(
        !same_acquired.exists(),
        "the same build key was acquired while its owner still held it"
    );
    same_key.assert_running();

    drop(owner_a);
    same_key.wait_success();
    assert!(same_acquired.is_file());
}

#[test]
fn abrupt_owner_exit_releases_the_kernel_lock_without_deleting_the_lock_file() {
    let temp = tempfile::tempdir().expect("temporary lock home");
    let owner_attempting = temp.path().join("owner-attempting");
    let owner_acquired = temp.path().join("owner-acquired");
    let mut owner = ManagedChild::spawn(
        temp.path(),
        "install",
        &owner_attempting,
        &owner_acquired,
        Duration::from_secs(60),
        None,
        None,
        None,
        "abrupt install-lock owner",
    );
    owner.wait_for_marker(&owner_attempting);
    owner.wait_for_marker(&owner_acquired);

    let status = owner.kill_and_wait();
    assert!(
        !status.success(),
        "the deliberately terminated owner unexpectedly exited successfully"
    );
    let lock_file = temp.path().join("locks").join("install.lock");
    assert!(
        lock_file.is_file(),
        "descriptor-lock files are stable rendezvous points, not stale-owner records"
    );

    let waiter_attempting = temp.path().join("post-crash-waiter-attempting");
    let waiter_acquired = temp.path().join("post-crash-waiter-acquired");
    let mut waiter = ManagedChild::spawn(
        temp.path(),
        "install",
        &waiter_attempting,
        &waiter_acquired,
        Duration::ZERO,
        None,
        None,
        None,
        "post-crash install-lock owner",
    );
    waiter.wait_for_marker(&waiter_attempting);
    waiter.wait_success();
    assert!(waiter_acquired.is_file());
    assert!(lock_file.is_file());
}
