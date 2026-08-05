use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use zed_lock::{LockClass, LockManager, LockRequest};

const ROLE: &str = "ZED_LOCK_TEST_ROLE";
const LOCK_PATH: &str = "ZED_LOCK_TEST_PATH";
const ATTEMPTING: &str = "ZED_LOCK_TEST_ATTEMPTING";
const ACQUIRED: &str = "ZED_LOCK_TEST_ACQUIRED";
const HOLD_MS: &str = "ZED_LOCK_TEST_HOLD_MS";
const COUNTER_PATH: &str = "ZED_LOCK_TEST_COUNTER";
const ITERATIONS: &str = "ZED_LOCK_TEST_ITERATIONS";
const HELPER_TEST: &str = "process_helper";
const DEADLINE: Duration = Duration::from_secs(15);

struct ManagedChild {
    child: Option<Child>,
    label: String,
}

impl ManagedChild {
    fn spawn(
        role: &str,
        lock_path: &Path,
        attempting: &Path,
        acquired: &Path,
        hold: Duration,
        counter: Option<&Path>,
        iterations: Option<usize>,
        label: impl Into<String>,
    ) -> Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg(HELPER_TEST)
            .arg("--exact")
            .arg("--nocapture")
            .env(ROLE, role)
            .env(LOCK_PATH, lock_path)
            .env(ATTEMPTING, attempting)
            .env(ACQUIRED, acquired)
            .env(HOLD_MS, hold.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(counter) = counter {
            command.env(COUNTER_PATH, counter);
        }
        if let Some(iterations) = iterations {
            command.env(ITERATIONS, iterations.to_string());
        }
        let label = label.into();
        let child = command
            .spawn()
            .with_context(|| format!("spawning {label}"))?;
        Ok(Self {
            child: Some(child),
            label,
        })
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("managed child still present")
    }

    fn wait_for_marker(&mut self, marker: &Path) -> Result<()> {
        let deadline = Instant::now() + DEADLINE;
        while !marker.is_file() {
            if let Some(status) = self.child_mut().try_wait()? {
                anyhow::bail!(
                    "{} exited before writing {}: {status}",
                    self.label,
                    marker.display()
                );
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "{} did not write {} before the deadline",
                    self.label,
                    marker.display()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn assert_running(&mut self) -> Result<()> {
        let status = self.child_mut().try_wait()?;
        anyhow::ensure!(
            status.is_none(),
            "{} exited unexpectedly: {status:?}",
            self.label
        );
        Ok(())
    }

    fn wait_success(&mut self) -> Result<()> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.child_mut().try_wait()? {
                self.child.take();
                anyhow::ensure!(status.success(), "{} failed: {status}", self.label);
                return Ok(());
            }
            if Instant::now() >= deadline {
                let mut child = self.child.take().expect("managed child still present");
                let _ = child.kill();
                let status = child.wait()?;
                anyhow::bail!("{} timed out; final status: {status}", self.label);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill_and_wait(&mut self) -> Result<ExitStatus> {
        let mut child = self.child.take().expect("managed child still present");
        child.kill()?;
        Ok(child.wait()?)
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
fn process_helper() -> Result<()> {
    let Some(role) = std::env::var_os(ROLE) else {
        return Ok(());
    };
    let role = role.to_string_lossy().into_owned();
    let lock_path = PathBuf::from(std::env::var_os(LOCK_PATH).context("helper lock path")?);
    let attempting = PathBuf::from(std::env::var_os(ATTEMPTING).context("attempting marker")?);
    let acquired = PathBuf::from(std::env::var_os(ACQUIRED).context("acquired marker")?);
    let hold_ms = std::env::var(HOLD_MS)?.parse::<u64>()?;

    fs::write(&attempting, b"attempting")?;
    let manager = LockManager::default();
    let request = || {
        LockRequest::exclusive(&lock_path)
            .operation(format!("process helper {role}"))
            .class(LockClass::ProjectMutation)
    };

    match role.as_str() {
        "hold" | "wait" => {
            let guard = manager.acquire_blocking(request())?;
            fs::write(&acquired, b"acquired")?;
            thread::sleep(Duration::from_millis(hold_ms));
            drop(guard);
        }
        "increment" => {
            let counter = PathBuf::from(
                std::env::var_os(COUNTER_PATH).context("counter path for increment helper")?,
            );
            let iterations = std::env::var(ITERATIONS)?.parse::<usize>()?;
            fs::write(&acquired, b"started")?;
            for _ in 0..iterations {
                let guard = manager.acquire_blocking(request())?;
                let current = fs::read_to_string(&counter)?.trim().parse::<usize>()?;
                fs::write(&counter, (current + 1).to_string())?;
                drop(guard);
            }
        }
        other => anyhow::bail!("unknown helper role: {other}"),
    }
    Ok(())
}

#[test]
fn contended_process_wakes_after_descriptor_release() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let lock_path = temp.path().join("handoff.lock");
    let owner = LockManager::default()
        .acquire_blocking(LockRequest::exclusive(&lock_path).operation("parent owner"))?;
    let attempting = temp.path().join("waiter-attempting");
    let acquired = temp.path().join("waiter-acquired");
    let mut waiter = ManagedChild::spawn(
        "wait",
        &lock_path,
        &attempting,
        &acquired,
        Duration::ZERO,
        None,
        None,
        "contended waiter",
    )?;

    waiter.wait_for_marker(&attempting)?;
    thread::sleep(Duration::from_millis(150));
    anyhow::ensure!(
        !acquired.exists(),
        "waiter acquired before the owner released the descriptor"
    );
    waiter.assert_running()?;

    drop(owner);
    waiter.wait_success()?;
    anyhow::ensure!(acquired.is_file(), "waiter never reported acquisition");
    Ok(())
}

#[test]
fn forced_owner_exit_releases_the_kernel_lock() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let lock_path = temp.path().join("owner-death.lock");
    let owner_attempting = temp.path().join("owner-attempting");
    let owner_acquired = temp.path().join("owner-acquired");
    let mut owner = ManagedChild::spawn(
        "hold",
        &lock_path,
        &owner_attempting,
        &owner_acquired,
        Duration::from_secs(60),
        None,
        None,
        "forced-exit owner",
    )?;
    owner.wait_for_marker(&owner_acquired)?;
    let status = owner.kill_and_wait()?;
    anyhow::ensure!(!status.success(), "terminated owner unexpectedly succeeded");
    anyhow::ensure!(
        lock_path.is_file(),
        "the stable lock-file rendezvous should not be deleted on owner exit"
    );

    let waiter_attempting = temp.path().join("post-exit-attempting");
    let waiter_acquired = temp.path().join("post-exit-acquired");
    let mut waiter = ManagedChild::spawn(
        "wait",
        &lock_path,
        &waiter_attempting,
        &waiter_acquired,
        Duration::ZERO,
        None,
        None,
        "post-exit waiter",
    )?;
    waiter.wait_success()?;
    anyhow::ensure!(waiter_acquired.is_file());
    Ok(())
}

#[test]
fn independent_lock_identities_progress_concurrently() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let held_path = temp.path().join("held.lock");
    let independent_path = temp.path().join("independent.lock");
    let owner = LockManager::default()
        .acquire_blocking(LockRequest::exclusive(&held_path).operation("unrelated owner"))?;
    let attempting = temp.path().join("independent-attempting");
    let acquired = temp.path().join("independent-acquired");
    let mut independent = ManagedChild::spawn(
        "wait",
        &independent_path,
        &attempting,
        &acquired,
        Duration::ZERO,
        None,
        None,
        "independent waiter",
    )?;

    independent.wait_success()?;
    anyhow::ensure!(acquired.is_file());
    drop(owner);
    Ok(())
}

#[test]
fn eight_processes_preserve_a_protected_counter() -> Result<()> {
    const PROCESS_COUNT: usize = 8;
    const ITERATIONS_PER_PROCESS: usize = 100;

    let temp = tempfile::tempdir()?;
    let lock_path = temp.path().join("counter.lock");
    let counter = temp.path().join("counter.txt");
    fs::write(&counter, b"0")?;

    let mut children = Vec::with_capacity(PROCESS_COUNT);
    for index in 0..PROCESS_COUNT {
        let attempting = temp.path().join(format!("counter-{index}-attempting"));
        let acquired = temp.path().join(format!("counter-{index}-started"));
        children.push(ManagedChild::spawn(
            "increment",
            &lock_path,
            &attempting,
            &acquired,
            Duration::ZERO,
            Some(&counter),
            Some(ITERATIONS_PER_PROCESS),
            format!("counter worker {index}"),
        )?);
    }
    for child in &mut children {
        child.wait_success()?;
    }

    let actual = fs::read_to_string(&counter)?.trim().parse::<usize>()?;
    assert_eq!(actual, PROCESS_COUNT * ITERATIONS_PER_PROCESS);
    Ok(())
}
