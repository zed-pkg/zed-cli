from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"expected one match in {path}, found {count}")
    file.write_text(text.replace(old, new, 1))


store = Path("src/store.rs")
text = store.read_text()
text = text.replace(
    "use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};",
    "use std::time::{Duration, SystemTime, UNIX_EPOCH};",
    1,
)

start_marker = "/// An advisory (flock-based) process lock held for the life of the guard."
end_marker = "/// What `Store::gc` did (or, with `dry_run`, would do)."
start = text.index(start_marker)
end = text.index(end_marker, start)
replacement = '''/// A descriptor-backed process lock held for the life of the guard.
///
/// Acquisition uses the operating system's blocking lock primitive directly:
/// `flock`/`fcntl` semantics on Unix and `LockFileEx` semantics on Windows via
/// `fs2`. Contended callers sleep in the kernel and wake when the owner drops
/// the descriptor or exits. There is no retry timer, jitter loop, stale-file
/// reclamation, or userspace polling.
pub struct ProcessLock {
    _file: fs::File,
}

impl ProcessLock {
    fn acquire(path: &Path, waiting_on: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening lock file {}", path.display()))?;

        file.lock_exclusive().with_context(|| {
            format!(
                "waiting for {waiting_on} through operating-system lock {}",
                path.display()
            )
        })?;
        Ok(Self { _file: file })
    }
}

'''
text = text[:start] + replacement + text[end:]

old_test_header = '''#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_survives_hostile_max_age() {
'''
new_test_header = '''#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    use super::*;

    #[test]
    fn install_process_lock_blocks_without_polling_until_owner_release() {
        const CHILD_HOME: &str = "ZED_PKG_TEST_STORE_LOCK_CHILD_HOME";
        const CHILD_ATTEMPTING: &str = "ZED_PKG_TEST_STORE_LOCK_CHILD_ATTEMPTING";
        const CHILD_ACQUIRED: &str = "ZED_PKG_TEST_STORE_LOCK_CHILD_ACQUIRED";
        const TEST_NAME: &str =
            "store::tests::install_process_lock_blocks_without_polling_until_owner_release";

        if let Some(home) = std::env::var_os(CHILD_HOME) {
            let attempting = PathBuf::from(std::env::var_os(CHILD_ATTEMPTING).unwrap());
            let acquired = PathBuf::from(std::env::var_os(CHILD_ACQUIRED).unwrap());
            fs::write(attempting, b"attempting").unwrap();
            let store = Store::new(Path::new(&home));
            let _waiter = store.install_lock().unwrap();
            fs::write(acquired, b"acquired").unwrap();
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        let owner = store.install_lock().unwrap();
        let attempting = temp.path().join("store-lock-waiter-attempting");
        let acquired = temp.path().join("store-lock-waiter-acquired");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg(TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_HOME, temp.path())
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
                panic!("store-lock waiter exited before attempting acquisition: {status}");
            }
            if Instant::now() >= attempting_deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("store-lock waiter did not reach acquisition");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !acquired.exists(),
            "a separate process acquired install.lock while the owner held it"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "store-lock waiter exited before owner release"
        );

        drop(owner);
        let release_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "store-lock waiter failed: {status}");
                break;
            }
            if Instant::now() >= release_deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("store-lock waiter did not wake after owner release");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(acquired.is_file(), "waiter exited without acquiring install.lock");
    }

    #[test]
    fn gc_survives_hostile_max_age() {
'''
if text.count(old_test_header) != 1:
    raise RuntimeError("could not locate store test module header")
text = text.replace(old_test_header, new_test_header, 1)
store.write_text(text)

replace_once(
    ".github/workflows/recursive-install-windows.yml",
    '''      - name: Prove failed downloads leave no cache publication
        run: cargo test --locked --manifest-path zed-cli/Cargo.toml --lib install_graph::tests::failed_downloads_remove_staging_files_and_never_publish_cache_entries -- --exact --nocapture
''',
    '''      - name: Prove failed downloads leave no cache publication
        run: cargo test --locked --manifest-path zed-cli/Cargo.toml --lib install_graph::tests::failed_downloads_remove_staging_files_and_never_publish_cache_entries -- --exact --nocapture

      - name: Prove the lower-level store lock blocks across processes
        run: cargo test --locked --manifest-path zed-cli/Cargo.toml --lib store::tests::install_process_lock_blocks_without_polling_until_owner_release -- --exact --nocapture
''',
)
