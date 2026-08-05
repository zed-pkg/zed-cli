from __future__ import annotations

import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def update_zed_lock_tests() -> None:
    path = ROOT / "crates" / "zed-lock" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")

    text = re.sub(
        r'''(?ms)^\s*let error = manager\s*\.try_acquire\(request\)\s*\.expect_err\("default same-process policy should reject reentry"\);''',
        '''        let error = match manager.try_acquire(request) {
            Ok(_) => panic!("default same-process policy should reject reentry"),
            Err(error) => error,
        };''',
        text,
        count=1,
    )
    text = re.sub(
        r'''(?ms)^\s*let error = manager\s*\.acquire\(\s*LockRequest::exclusive\(temp\.path\(\)\.join\("unrelated\.lock"\)\)\s*\.operation\("over-cap waiter"\),?\s*\)\s*\.expect_err\("second waiter should be rejected at the configured cap"\);''',
        '''        let error = match manager.acquire(
            LockRequest::exclusive(temp.path().join("unrelated.lock"))
                .operation("over-cap waiter"),
        ) {
            Ok(_) => panic!("second waiter should be rejected at the configured cap"),
            Err(error) => error,
        };''',
        text,
        count=1,
    )
    text = text.replace(
        'assert!(error.to_string().contains("waiter limit reached"));',
        'assert!(format!("{error:#}").contains("waiter limit reached"));',
        1,
    )

    forbidden = (
        '.expect_err("default same-process policy should reject reentry")',
        '.expect_err("second waiter should be rejected at the configured cap")',
    )
    remaining = [needle for needle in forbidden if needle in text]
    if remaining:
        raise RuntimeError(f"test assertions were not migrated: {remaining}")

    path.write_text(text, encoding="utf-8")


def update_store() -> None:
    path = ROOT / "src" / "store.rs"
    text = path.read_text(encoding="utf-8")

    text = text.replace(
        "use fs2::FileExt;\n",
        "use zed_lock::{LockClass, LockManager, LockRequest};\n",
        1,
    )

    old_definition = '''/// A descriptor-backed process lock held for the life of the guard.
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
    new_definition = '''/// Compatibility name for the descriptor-backed guard supplied by `zed-lock`.
pub type ProcessLock = zed_lock::LockGuard;

fn acquire_process_lock(path: &Path, waiting_on: &str, class: LockClass) -> Result<ProcessLock> {
    LockManager::global().acquire_blocking(
        LockRequest::exclusive(path)
            .operation(waiting_on)
            .class(class)
            // Store workers are independent tasks that may intentionally
            // contend inside one process. The kernel remains authoritative.
            .queue_same_process(),
    )
}
'''
    text = text.replace(old_definition, new_definition, 1)

    text = re.sub(
        r'''(?ms)        ProcessLock::acquire\(\s*&self\s*\.locks_dir\(\)\s*\.join\(format!\("build-\{platform\}-\{key\}\.lock"\)\),\s*&format!\("the build of \{key\}"\),\s*\)''',
        '''        acquire_process_lock(
            &self
                .locks_dir()
                .join(format!("build-{platform}-{key}.lock")),
            &format!("the build of {key}"),
            LockClass::Build,
        )''',
        text,
        count=1,
    )
    text = text.replace(
        '        ProcessLock::acquire(&self.locks_dir().join("install.lock"), "the install lock")\n',
        '''        acquire_process_lock(
            &self.locks_dir().join("install.lock"),
            "the install lock",
            LockClass::ProjectMutation,
        )
''',
        1,
    )
    text = re.sub(
        r'''(?ms)        let _lock = ProcessLock::acquire\(\s*&self\.locks_dir\(\)\.join\(format!\("\{expected_sha256\}\.lock"\)\),\s*&format!\("extraction of \{expected_sha256\}"\),\s*\)\?;''',
        '''        let _lock = acquire_process_lock(
            &self.locks_dir().join(format!("{expected_sha256}.lock")),
            &format!("extraction of {expected_sha256}"),
            LockClass::Artifact,
        )?;''',
        text,
        count=1,
    )

    if "pub struct ProcessLock" in text or "ProcessLock::acquire" in text:
        raise RuntimeError("zed-cli Store was not fully migrated to zed-lock")

    path.write_text(text, encoding="utf-8")


def update_manifest() -> None:
    path = ROOT / "Cargo.toml"
    text = path.read_text(encoding="utf-8")
    text = text.replace('fs2 = "0.4.3"\n', "", 1)
    path.write_text(text, encoding="utf-8")


def main() -> None:
    update_zed_lock_tests()
    update_store()
    update_manifest()


if __name__ == "__main__":
    main()
