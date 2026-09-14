use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zed_cli::store::Store;

const HELPER_ROLE: &str = "ZED_DEN2037_HELPER_ROLE";
const HELPER_HOME: &str = "ZED_DEN2037_HELPER_HOME";
const HELPER_CRITICAL: &str = "ZED_DEN2037_HELPER_CRITICAL";
const HELPER_OVERLAP: &str = "ZED_DEN2037_HELPER_OVERLAP";
const HELPER_HOLD_MS: &str = "ZED_DEN2037_HELPER_HOLD_MS";
const HELPER_TEST: &str = "den2037_install_lock_helper";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(home: &Path, critical: &Path, overlap: &Path, hold_ms: u64) -> Self {
        let child = Command::new(std::env::current_exe().expect("current integration test binary"))
            .arg(HELPER_TEST)
            .arg("--exact")
            .arg("--nocapture")
            .env(HELPER_ROLE, "install")
            .env(HELPER_HOME, home)
            .env(HELPER_CRITICAL, critical)
            .env(HELPER_OVERLAP, overlap)
            .env(HELPER_HOLD_MS, hold_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn DEN-2037 lock helper");
        Self(Some(child))
    }

    fn wait_success(&mut self) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let child = self.0.as_mut().expect("child present");
            if let Some(status) = child.try_wait().expect("poll helper") {
                self.0.take();
                assert!(status.success(), "DEN-2037 helper failed: {status}");
                return;
            }
            if Instant::now() >= deadline {
                let mut child = self.0.take().expect("child present");
                let _ = child.kill();
                let status = child.wait().expect("reap timed-out helper");
                panic!("DEN-2037 helper timed out; final status: {status}");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn den2037_install_lock_helper() {
    if std::env::var_os(HELPER_ROLE).is_none() {
        return;
    }
    let home = PathBuf::from(std::env::var_os(HELPER_HOME).expect("helper home"));
    let critical = PathBuf::from(std::env::var_os(HELPER_CRITICAL).expect("critical marker"));
    let overlap = PathBuf::from(std::env::var_os(HELPER_OVERLAP).expect("overlap marker"));
    let hold_ms = std::env::var(HELPER_HOLD_MS)
        .expect("hold ms")
        .parse::<u64>()
        .expect("numeric hold ms");

    let store = Store::new(&home);
    let guard = store.install_lock().expect("acquire install lock");
    if let Err(error) = fs::create_dir(&critical) {
        let _ = fs::write(&overlap, format!("critical section overlap: {error}\n"));
        panic!("two processes entered the install critical section: {error}");
    }
    thread::sleep(Duration::from_millis(hold_ms));
    fs::remove_dir(&critical).expect("remove critical marker");
    drop(guard);
}

#[test]
fn install_lock_scales_across_2_4_8_16_processes_without_overlap() {
    for contenders in [2usize, 4, 8, 16] {
        let temp = tempfile::tempdir().expect("temporary root");
        let home = temp.path().join("home");
        let critical = temp.path().join("critical");
        let overlap = temp.path().join("overlap");
        let started = Instant::now();
        let mut children = (0..contenders)
            .map(|_| ChildGuard::spawn(&home, &critical, &overlap, 8))
            .collect::<Vec<_>>();
        for child in &mut children {
            child.wait_success();
        }
        assert!(
            !overlap.exists(),
            "{contenders} contenders overlapped the protected section"
        );
        assert!(
            !critical.exists(),
            "{contenders} contenders left a critical-section marker behind"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{contenders} contenders exceeded the coarse local contention tail budget"
        );
    }
}

#[test]
fn clean_home_bootstraps_lock_root_without_destroying_unrelated_content() {
    let temp = tempfile::tempdir().expect("temporary root");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    let unrelated = home.join("keep-me.txt");
    fs::write(&unrelated, b"keep").expect("write unrelated content");

    let store = Store::new(&home);
    let guard = store.install_lock().expect("acquire install lock");
    let lock_root = home.join("locks");
    assert!(lock_root.is_dir(), "lock root was not bootstrapped");
    assert!(lock_root.join("install.lock").is_file());
    assert_eq!(fs::read(&unrelated).expect("read unrelated content"), b"keep");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&lock_root)
            .expect("lock-root metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o077, 0, "lock root must not expose group/other permission bits: {mode:o}");
    }

    drop(guard);
    assert!(unrelated.is_file());
}

#[test]
fn unknown_legacy_lock_artifacts_are_preserved() {
    let temp = tempfile::tempdir().expect("temporary root");
    let home = temp.path().join("home");
    let lock_root = home.join("locks");
    fs::create_dir_all(&lock_root).expect("create lock root");
    let legacy = lock_root.join("legacy-v0.lockdir");
    fs::create_dir(&legacy).expect("seed unknown legacy lock artifact");
    fs::write(legacy.join("opaque"), b"do-not-delete").expect("seed legacy payload");

    let store = Store::new(&home);
    let guard = store.install_lock().expect("acquire install lock");
    drop(guard);

    assert_eq!(
        fs::read(legacy.join("opaque")).expect("legacy artifact preserved"),
        b"do-not-delete"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_home_contends_with_real_home() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temporary root");
    let real_home = temp.path().join("real-home");
    let alias_home = temp.path().join("alias-home");
    fs::create_dir_all(&real_home).expect("create real home");
    symlink(&real_home, &alias_home).expect("symlink home");

    let owner = Store::new(&real_home)
        .install_lock()
        .expect("real-home owner lock");
    let critical = temp.path().join("alias-critical");
    let overlap = temp.path().join("alias-overlap");
    let mut child = ChildGuard::spawn(&alias_home, &critical, &overlap, 0);

    thread::sleep(Duration::from_millis(150));
    assert!(
        !critical.exists(),
        "symlinked home bypassed canonical lock identity while real home was held"
    );
    drop(owner);
    child.wait_success();
    assert!(!overlap.exists());
}
