use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use zed_interfaces::manifest::is_sha256_hex;
use zed_interfaces::paths::{ARCHIVE_ROOT, STORE_PKG_DIR, store_entry_rel};

use crate::pack::sha256_file;

/// Ceiling on the bytes an artifact may expand to, guarding against
/// decompression bombs. Override with `ZED_PKG_MAX_UNPACKED_BYTES`.
const DEFAULT_MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Ceiling on archive entry count (inode-exhaustion guard).
const MAX_ARCHIVE_ENTRIES: usize = 200_000;

fn max_unpacked_bytes() -> u64 {
    std::env::var("ZED_PKG_MAX_UNPACKED_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_UNPACKED_BYTES)
}

/// Registry responses, lockfiles, and cache lookups all feed digests into
/// filesystem paths — reject anything that is not a plain hex digest before
/// it can traverse anywhere.
pub fn require_sha256(sha256: &str) -> Result<()> {
    if !is_sha256_hex(sha256) {
        bail!("invalid sha256 digest `{sha256}` (expected 64 lowercase hex chars)");
    }
    Ok(())
}

/// An advisory (flock-based) process lock held for the life of the guard.
/// CLI commands are highly concurrent — two `zed install` runs in different
/// terminals, or N parallel CI runners — so store mutations that must not
/// interleave (extracting the same artifact, rewriting refs.json) take a
/// lock first. The OS releases it if the process dies, so a crash can't
/// wedge the store.
pub struct ProcessLock {
    _file: fs::File,
}

impl ProcessLock {
    /// Three-phase acquisition tuned for CLI latency:
    /// spin-wait at 50ms while young (catches sub-second releases with no
    /// perceptible lag), then exponential backoff with full jitter so
    /// parallel CI runners don't wake in lockstep, and after 3s tell the
    /// user what they are waiting on instead of freezing silently.
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

        let timeout = Duration::from_secs(
            std::env::var("ZED_PKG_LOCK_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        );
        let start = Instant::now();
        let mut backoff_attempt: u32 = 0;
        let mut notified = false;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(_) if start.elapsed() >= timeout => {
                    bail!(
                        "timed out after {}s waiting for {waiting_on} (lock {}); \
                         another zed process may be stuck — raise ZED_PKG_LOCK_TIMEOUT \
                         or investigate",
                        timeout.as_secs(),
                        path.display()
                    );
                }
                Err(_) => {}
            }
            let elapsed = start.elapsed();
            if elapsed >= Duration::from_secs(3) && !notified {
                eprintln!("waiting for {waiting_on} (held by another zed process)...");
                notified = true;
            }
            let sleep = if elapsed < Duration::from_millis(500) {
                Duration::from_millis(50)
            } else {
                backoff_attempt = backoff_attempt.saturating_add(1);
                let cap_ms: u64 = 2000;
                let exp_ms = 100u64
                    .saturating_mul(1u64 << backoff_attempt.min(20))
                    .min(cap_ms);
                // Full jitter: uniform in [0, exp_ms] without pulling in an
                // RNG dependency — subsec_nanos is plenty for de-syncing.
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64)
                    .unwrap_or(0);
                Duration::from_millis(nanos % (exp_ms + 1))
            };
            std::thread::sleep(sleep);
        }
    }
}

/// The global content-addressed store under `$HOME/.zed-pkg`. One extracted
/// copy per artifact per machine; projects symlink (or copy) out of it.
pub struct Store {
    home: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Refs {
    /// project absolute path -> sha256s it references
    #[serde(default)]
    projects: BTreeMap<String, Vec<String>>,
}

impl Store {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.home.join("store")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.home.join("cache")
    }

    pub fn cached_artifact(&self, sha256: &str) -> PathBuf {
        self.cache_dir().join(format!("{sha256}.tar.gz"))
    }

    pub fn entry_dir(&self, sha256: &str) -> PathBuf {
        self.home.join(store_entry_rel(sha256))
    }

    /// Directory holding the package files for an artifact.
    pub fn pkg_dir(&self, sha256: &str) -> PathBuf {
        self.entry_dir(sha256).join(STORE_PKG_DIR)
    }

    pub fn has(&self, sha256: &str) -> bool {
        self.pkg_dir(sha256).is_dir()
    }

    fn locks_dir(&self) -> PathBuf {
        self.home.join("locks")
    }

    /// Serializes the whole install (refs.json + lockfile writes) against
    /// other zed processes. Held by the caller for the duration of install.
    pub fn install_lock(&self) -> Result<ProcessLock> {
        ProcessLock::acquire(&self.locks_dir().join("install.lock"), "the install lock")
    }

    /// Serializes one artifact's build (per sha + platform) across processes.
    pub fn build_lock(&self, platform: &str, sha256: &str) -> Result<ProcessLock> {
        require_sha256(sha256)?;
        ProcessLock::acquire(
            &self.locks_dir().join(format!("build-{platform}-{sha256}.lock")),
            &format!("the build of {sha256}"),
        )
    }

    /// Verify the archive hash and extract it into the store. Idempotent and
    /// safe under concurrency: a per-sha flock means only one process
    /// extracts a given artifact, and extraction still goes via a temp dir
    /// and atomic rename as a second line of defense.
    pub fn add_artifact(&self, archive: &Path, expected_sha256: &str) -> Result<PathBuf> {
        require_sha256(expected_sha256)?;
        let (actual, _) = sha256_file(archive)?;
        if actual != expected_sha256 {
            bail!(
                "artifact hash mismatch: expected {expected_sha256}, got {actual} ({})",
                archive.display()
            );
        }
        let entry = self.entry_dir(expected_sha256);
        if self.has(expected_sha256) {
            self.touch_last_used(expected_sha256);
            return Ok(entry.join(STORE_PKG_DIR));
        }
        // Only one process extracts this sha at a time; the rest wait here
        // and then see has()==true.
        let _lock = ProcessLock::acquire(
            &self.locks_dir().join(format!("{expected_sha256}.lock")),
            &format!("extraction of {expected_sha256}"),
        )?;
        if self.has(expected_sha256) {
            self.touch_last_used(expected_sha256);
            return Ok(entry.join(STORE_PKG_DIR));
        }
        let parent = entry
            .parent()
            .context("store entry has a parent")?
            .to_path_buf();
        fs::create_dir_all(&parent)?;
        let tmp = tempfile::tempdir_in(&parent)?;

        // Detect the format by magic bytes rather than trusting a filename:
        // gzip starts with 1f 8b, zip with "PK\x03\x04". Both are supported.
        extract_archive(archive, tmp.path())?;
        // Archives root files under `pkg/`, which matches STORE_PKG_DIR.
        if !tmp.path().join(ARCHIVE_ROOT).is_dir() {
            bail!(
                "invalid artifact: missing `{ARCHIVE_ROOT}/` root in {}",
                archive.display()
            );
        }
        let tmp_path = tmp.keep();
        match fs::rename(&tmp_path, &entry) {
            Ok(()) => {}
            Err(_) if entry.exists() => {
                // Lost a race with a concurrent install; theirs is fine.
                let _ = fs::remove_dir_all(&tmp_path);
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp_path);
                return Err(e.into());
            }
        }
        self.touch_last_used(expected_sha256);
        Ok(entry.join(STORE_PKG_DIR))
    }

    /// Record that an artifact was used, for `zed gc`'s age policy. Lives
    /// next to (not inside) the immutable `pkg/` tree. Best-effort:
    /// filesystems with frozen clocks just fall back to dir mtimes.
    fn touch_last_used(&self, sha256: &str) {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = fs::write(self.entry_dir(sha256).join(".last-used"), stamp.to_string());
    }

    fn last_used(&self, entry: &Path) -> Option<SystemTime> {
        let text = fs::read_to_string(entry.join(".last-used")).ok()?;
        let secs: u64 = text.trim().parse().ok()?;
        Some(UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn refs_path(&self) -> PathBuf {
        self.home.join("refs.json")
    }

    fn load_refs(&self) -> Refs {
        fs::read_to_string(self.refs_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save_refs(&self, refs: &Refs) -> Result<()> {
        fs::create_dir_all(&self.home)?;
        fs::write(self.refs_path(), serde_json::to_string_pretty(refs)?)?;
        Ok(())
    }

    /// Record which artifacts a project references (called on install).
    pub fn record_project(&self, project: &Path, sha256s: Vec<String>) -> Result<()> {
        let mut refs = self.load_refs();
        refs.projects
            .insert(project.to_string_lossy().to_string(), sha256s);
        self.save_refs(&refs)
    }

    /// Drop refs to deleted projects, then delete unreferenced store
    /// entries and cached artifacts. Returns (entries_removed, bytes_freed).
    pub fn prune(&self) -> Result<(usize, u64)> {
        let mut refs = self.load_refs();
        refs.projects
            .retain(|project, _| Path::new(project).is_dir());
        let referenced: BTreeSet<String> = refs.projects.values().flatten().cloned().collect();
        self.save_refs(&refs)?;

        let mut removed = 0usize;
        let mut freed = 0u64;
        let version_root = self.root().join(zed_interfaces::paths::STORE_VERSION);
        if version_root.is_dir() {
            for shard in fs::read_dir(&version_root)? {
                let shard = shard?.path();
                if !shard.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(&shard)? {
                    let entry = entry?.path();
                    let sha = entry
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !referenced.contains(&sha) {
                        freed += dir_size(&entry);
                        fs::remove_dir_all(&entry)?;
                        removed += 1;
                        let cached = self.cached_artifact(&sha);
                        if cached.exists() {
                            freed += cached.metadata().map(|m| m.len()).unwrap_or(0);
                            let _ = fs::remove_file(cached);
                        }
                    }
                }
            }
        }
        Ok((removed, freed))
    }

    /// Age-aware garbage collection (issue: enterprise-scale cache bloat).
    /// Removes store entries that are BOTH unreferenced by any live project
    /// AND unused for `max_age_days` (LRU via the `.last-used` marker,
    /// falling back to directory mtime), plus cached artifact downloads
    /// older than the cutoff (those are always safely re-downloadable).
    /// `max_age_days = 0` collects every unreferenced entry immediately.
    /// Returns (entries_removed, cache_files_removed, bytes_freed).
    pub fn gc(&self, max_age_days: u64) -> Result<(usize, usize, u64)> {
        let mut refs = self.load_refs();
        refs.projects
            .retain(|project, _| Path::new(project).is_dir());
        let referenced: BTreeSet<String> = refs.projects.values().flatten().cloned().collect();
        self.save_refs(&refs)?;

        let cutoff = SystemTime::now() - Duration::from_secs(max_age_days * 24 * 60 * 60);
        let too_old = |entry: &Path| -> bool {
            let last = self
                .last_used(entry)
                .or_else(|| entry.metadata().and_then(|m| m.modified()).ok());
            last.is_none_or(|t| t <= cutoff)
        };

        let mut entries_removed = 0usize;
        let mut freed = 0u64;
        let version_root = self.root().join(zed_interfaces::paths::STORE_VERSION);
        if version_root.is_dir() {
            for shard in fs::read_dir(&version_root)? {
                let shard = shard?.path();
                if !shard.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(&shard)? {
                    let entry = entry?.path();
                    let sha = entry
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !referenced.contains(&sha) && too_old(&entry) {
                        freed += dir_size(&entry);
                        fs::remove_dir_all(&entry)?;
                        entries_removed += 1;
                    }
                }
            }
        }

        let mut cache_removed = 0usize;
        if self.cache_dir().is_dir() {
            for file in fs::read_dir(self.cache_dir())? {
                let file = file?.path();
                let old = file
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t <= cutoff)
                    .unwrap_or(true);
                if old {
                    freed += file.metadata().map(|m| m.len()).unwrap_or(0);
                    let _ = fs::remove_file(&file);
                    cache_removed += 1;
                }
            }
        }
        Ok((entries_removed, cache_removed, freed))
    }

    pub fn status(&self) -> (usize, u64, u64) {
        let mut count = 0usize;
        let version_root = self.root().join(zed_interfaces::paths::STORE_VERSION);
        if let Ok(shards) = fs::read_dir(&version_root) {
            for shard in shards.flatten() {
                if let Ok(entries) = fs::read_dir(shard.path()) {
                    count += entries.count();
                }
            }
        }
        (count, dir_size(&self.root()), dir_size(&self.cache_dir()))
    }

    pub fn clean_cache(&self) -> Result<u64> {
        let dir = self.cache_dir();
        let freed = dir_size(&dir);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(freed)
    }
}

/// Extract a `tar.gz` or `zip` artifact into `dest`, detected by magic bytes.
fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let mut magic = [0u8; 4];
    let read = {
        use std::io::Read as _;
        let mut file = fs::File::open(archive)?;
        file.read(&mut magic).unwrap_or(0)
    };
    if read >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        let file = fs::File::open(archive)?;
        let mut tar = tar::Archive::new(GzDecoder::new(file));
        tar.unpack(dest)?;
        Ok(())
    } else if read >= 4 && &magic == b"PK\x03\x04" {
        let file = fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(file)
            .with_context(|| format!("reading zip {}", archive.display()))?;
        zip.extract(dest)?;
        Ok(())
    } else {
        bail!(
            "unrecognized artifact format in {} (expected gzip or zip)",
            archive.display()
        );
    }
}

pub fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
