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
use zed_interfaces::paths::{ARCHIVE_ROOT, STORE_PKG_DIR, build_entry_rel, store_entry_rel};

use crate::pack::sha256_file;

/// Ceiling on the bytes an artifact may expand to, guarding against
/// decompression bombs. Override with `ZED_PKG_MAX_UNPACKED_BYTES`.
const DEFAULT_MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Ceiling on archive entry count (inode-exhaustion guard).
const MAX_ARCHIVE_ENTRIES: usize = 200_000;
/// Ceiling on `zed gc --max-age-days`, defending the age computation against
/// hostile input. ~10,000 years is far past any real cache lifetime while
/// leaving the seconds conversion well clear of `u64` overflow.
/// Only exercised by the hostile-age gc test since parse_age saturates.
#[cfg(test)]
const MAX_GC_AGE_DAYS: u64 = 3_650_000;

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

/// Build-cache keys are `<sha256>` or `<sha256>-<command-hash>`; anything
/// outside lowercase hex plus `-` could traverse, so reject it.
fn require_build_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c) || c == '-')
    {
        bail!("invalid build cache key `{key}`");
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

/// What `Store::gc` did (or, with `dry_run`, would do).
#[derive(Debug)]
pub struct GcReport {
    pub entries_removed: usize,
    pub cache_files_removed: usize,
    pub freed: u64,
    pub dry_run: bool,
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

    // --- build cache (zed-docs issue #5) ----------------------------------

    /// Root of the per-platform build cache. Kept separate from the source
    /// store so the source store stays platform-independent and shareable,
    /// while compiled output is keyed by platform and build command.
    pub fn builds_root(&self) -> PathBuf {
        self.home.join("builds")
    }

    /// Build-cache entry for a `(platform, key)` pair, where `key` is the
    /// source sha256, optionally suffixed with a hash of the build command
    /// and outputs so a consumer override never collides with the package's
    /// own build.
    pub fn build_entry(&self, platform: &str, key: &str) -> PathBuf {
        self.home.join(build_entry_rel(platform, key))
    }

    pub fn build_pkg_dir(&self, platform: &str, key: &str) -> PathBuf {
        self.build_entry(platform, key).join(STORE_PKG_DIR)
    }

    pub fn has_build(&self, platform: &str, key: &str) -> bool {
        self.build_pkg_dir(platform, key).is_dir()
    }

    pub fn build_size(&self) -> u64 {
        dir_size(&self.builds_root())
    }

    /// Serializes one artifact's build (per platform + key) across processes.
    pub fn build_lock(&self, platform: &str, key: &str) -> Result<ProcessLock> {
        require_build_key(key)?;
        ProcessLock::acquire(
            &self
                .locks_dir()
                .join(format!("build-{platform}-{key}.lock")),
            &format!("the build of {key}"),
        )
    }

    /// Serializes the whole install (refs.json + lockfile writes) against
    /// other zed processes. Held by the caller for the duration of install.
    pub fn install_lock(&self) -> Result<ProcessLock> {
        ProcessLock::acquire(&self.locks_dir().join("install.lock"), "the install lock")
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
        if let Ok(text) = fs::read_to_string(entry.join(".last-used"))
            && let Ok(secs) = text.trim().parse::<u64>()
        {
            return Some(UNIX_EPOCH + Duration::from_secs(secs));
        }
        entry.metadata().and_then(|m| m.modified()).ok()
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

    /// Age-aware, LRU-style garbage collection (zed-docs issue #7 /
    /// enterprise-scale cache bloat). Removes store entries that are BOTH
    /// unreferenced by any live project AND unused past `max_age` (via the
    /// `.last-used` marker, falling back to directory mtime), sweeps the
    /// per-platform build cache by the same age policy (build output is
    /// always re-buildable), and drops cached downloads older than the
    /// cutoff (always safely re-downloadable). With `dry_run`, reports what
    /// would be removed without mutating anything (refs included).
    pub fn gc(&self, max_age: Duration, dry_run: bool) -> Result<GcReport> {
        let mut refs = self.load_refs();
        refs.projects
            .retain(|project, _| Path::new(project).is_dir());
        let referenced: BTreeSet<String> = refs.projects.values().flatten().cloned().collect();
        if !dry_run {
            self.save_refs(&refs)?;
        }

        // `max_age` originates from unvalidated CLI/env input (parse_age uses
        // a saturating multiply), and checked_sub falls back to the epoch so
        // hostile ages can never panic — they just prune nothing extra.
        let cutoff = SystemTime::now().checked_sub(max_age).unwrap_or(UNIX_EPOCH);
        // Prefer the recorded last-use stamp; fall back to filesystem mtime so
        // entries predating use-tracking age out instead of being treated as
        // immediately collectable.
        let too_old = |entry: &Path| -> bool {
            let last = self
                .last_used(entry)
                .or_else(|| entry.metadata().and_then(|m| m.modified()).ok());
            last.is_none_or(|t| t <= cutoff)
        };

        let mut entries_removed = 0usize;
        let mut freed = 0u64;
        // Source store: store/v1/<shard>/<sha> — guarded by live references.
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
                        if !dry_run {
                            fs::remove_dir_all(&entry)?;
                        }
                        entries_removed += 1;
                    }
                }
            }
        }
        // Build cache: builds/v1/<platform>/<shard>/<key> — age-only (a
        // future install just rebuilds).
        let builds_root = self
            .builds_root()
            .join(zed_interfaces::paths::STORE_VERSION);
        if builds_root.is_dir() {
            for platform in fs::read_dir(&builds_root)? {
                let platform = platform?.path();
                if !platform.is_dir() {
                    continue;
                }
                for shard in fs::read_dir(&platform)? {
                    let shard = shard?.path();
                    if !shard.is_dir() {
                        continue;
                    }
                    for entry in fs::read_dir(&shard)? {
                        let entry = entry?.path();
                        if entry.is_dir() && too_old(&entry) {
                            freed += dir_size(&entry);
                            if !dry_run {
                                fs::remove_dir_all(&entry)?;
                            }
                            entries_removed += 1;
                        }
                    }
                }
            }
        }

        let mut cache_files_removed = 0usize;
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
                    if !dry_run {
                        let _ = fs::remove_file(&file);
                    }
                    cache_files_removed += 1;
                }
            }
        }
        Ok(GcReport {
            entries_removed,
            cache_files_removed,
            freed,
            dry_run,
        })
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

/// Self-update extracts a release archive it just downloaded over TLS from
/// GitHub; reuse the same hardened extractor as store artifacts.
pub fn extract_archive_for_update(archive: &Path, dest: &Path) -> Result<()> {
    extract_archive(archive, dest)
}

/// True when an archive-declared path is safe to create under `dest`:
/// relative, no `..`, no absolute/prefix components.
fn safe_entry_path(path: &Path) -> bool {
    path.components().all(|c| matches!(c, Component::Normal(_))) && !path.as_os_str().is_empty()
}

/// Extract a `tar.gz` or `zip` artifact into `dest`, detected by magic
/// bytes. Every entry is screened before touching the disk: traversal
/// paths, symlinks/hardlinks, and non-file specials are rejected, and the
/// total decompressed size is capped — archives come from the network, so
/// the extractor is a security boundary, not a convenience.
fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let mut magic = [0u8; 4];
    let read = {
        let mut file = fs::File::open(archive)?;
        file.read(&mut magic).unwrap_or(0)
    };
    let budget = max_unpacked_bytes();
    let mut written: u64 = 0;
    let mut entries: usize = 0;

    let mut charge = |bytes: u64, entries: &mut usize| -> Result<()> {
        *entries += 1;
        if *entries > MAX_ARCHIVE_ENTRIES {
            bail!("artifact has more than {MAX_ARCHIVE_ENTRIES} entries; refusing");
        }
        written = written.saturating_add(bytes);
        if written > budget {
            bail!(
                "artifact expands past the {budget}-byte cap \
                 (set ZED_PKG_MAX_UNPACKED_BYTES to raise it); refusing"
            );
        }
        Ok(())
    };

    if read >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        let file = fs::File::open(archive)?;
        let mut tar = tar::Archive::new(GzDecoder::new(file));
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_path_buf();
            if !safe_entry_path(&path) {
                bail!(
                    "artifact entry `{}` escapes the extraction root",
                    path.display()
                );
            }
            let kind = entry.header().entry_type();
            match kind {
                tar::EntryType::Regular | tar::EntryType::Directory => {}
                other => bail!(
                    "artifact entry `{}` has unsupported type {other:?} \
                     (only files and directories are allowed)",
                    path.display()
                ),
            }
            charge(entry.header().size().unwrap_or(0), &mut entries)?;
            let target = dest.join(&path);
            if kind == tar::EntryType::Directory {
                fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            // take(size+1): a lying header cannot stream more than declared.
            let declared = entry.header().size().unwrap_or(0);
            let mut limited = (&mut entry).take(declared.saturating_add(1));
            let mut out = fs::File::create(&target)?;
            let copied = std::io::copy(&mut limited, &mut out)?;
            if copied > declared {
                bail!(
                    "artifact entry `{}` exceeds its declared size",
                    path.display()
                );
            }
            #[cfg(unix)]
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode));
            }
        }
        Ok(())
    } else if read >= 4 && &magic == b"PK\x03\x04" {
        let file = fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(file)
            .with_context(|| format!("reading zip {}", archive.display()))?;
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            // enclosed_name() is the zip crate's traversal-safe accessor.
            let Some(path) = entry.enclosed_name() else {
                bail!("zip entry `{}` escapes the extraction root", entry.name());
            };
            if !safe_entry_path(&path) {
                bail!("zip entry `{}` escapes the extraction root", entry.name());
            }
            if entry.is_symlink() {
                bail!("zip entry `{}` is a symlink; refusing", entry.name());
            }
            charge(entry.size(), &mut entries)?;
            let target = dest.join(&path);
            if entry.is_dir() {
                fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let declared = entry.size();
            let mut limited = (&mut entry).take(declared.saturating_add(1));
            let mut out = fs::File::create(&target)?;
            let copied = std::io::copy(&mut limited, &mut out)?;
            if copied > declared {
                bail!("zip entry `{}` exceeds its declared size", target.display());
            }
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_survives_hostile_max_age() {
        // A hostile age (`u64::MAX` seconds) would overflow naive epoch math
        // and panic the `SystemTime - Duration` subtraction; the hardened
        // path (saturating parse_age + checked_sub falling back to the epoch)
        // must simply prune nothing.
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let report = store
            .gc(Duration::from_secs(u64::MAX), false)
            .expect("gc must not fail on an empty store");
        assert_eq!(
            (
                report.entries_removed,
                report.cache_files_removed,
                report.freed
            ),
            (0, 0, 0)
        );

        // A handful of other extreme ages must be equally panic-free.
        for days in [0u64, 1, MAX_GC_AGE_DAYS, u64::MAX / 2, u64::MAX - 1] {
            let age = Duration::from_secs(days.saturating_mul(86_400));
            store.gc(age, false).expect("gc must not panic for any age");
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Create a store entry for `sha` whose `.last-used` stamp is `age_secs`
    /// in the past. The directory itself is freshly created, so any test that
    /// sees it pruned has also proven the stamp beats the (fresh) mtime.
    fn seed_entry(store: &Store, sha: &str, age_secs: u64) -> std::path::PathBuf {
        let dir = store.entry_dir(sha);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("payload"), b"0123456789").unwrap();
        fs::write(dir.join(".last-used"), (now_secs() - age_secs).to_string()).unwrap();
        dir
    }

    #[test]
    fn gc_prunes_by_stamp_age_but_spares_referenced_and_fresh_entries() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let old_sha = "a".repeat(64);
        let fresh_sha = "b".repeat(64);
        let referenced_sha = "c".repeat(64);
        let garbage_stamp_sha = "d".repeat(64);

        let old = seed_entry(&store, &old_sha, 7_200);
        let fresh = seed_entry(&store, &fresh_sha, 0);
        let referenced = seed_entry(&store, &referenced_sha, 7_200);
        // An unparsable stamp must fall back to the (fresh) dir mtime, not be
        // treated as immediately collectable.
        let garbage = seed_entry(&store, &garbage_stamp_sha, 0);
        fs::write(garbage.join(".last-used"), "not-a-timestamp").unwrap();

        // A live project pins `referenced_sha` even though it is old.
        let project = tempfile::tempdir().unwrap();
        store
            .record_project(project.path(), vec![referenced_sha.clone()])
            .unwrap();

        // Aged build-cache entry: age-only policy, no reference protection.
        let build = store
            .builds_root()
            .join(zed_interfaces::paths::STORE_VERSION)
            .join("test-platform")
            .join("aa")
            .join("buildkey");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("out"), b"bin").unwrap();
        fs::write(build.join(".last-used"), (now_secs() - 7_200).to_string()).unwrap();

        // Dry run: full report, zero mutation.
        let dry = store.gc(Duration::from_secs(3_600), true).unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.entries_removed, 2, "old store entry + aged build entry");
        assert!(dry.freed > 0);
        assert!(old.exists() && build.exists(), "dry run must not delete");

        // Real run: prunes exactly the same set.
        let report = store.gc(Duration::from_secs(3_600), false).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.entries_removed, 2);
        assert_eq!(report.freed, dry.freed);
        assert!(!old.exists(), "old unreferenced entry is pruned");
        assert!(!build.exists(), "aged build entry is pruned");
        assert!(fresh.exists(), "fresh entry survives");
        assert!(referenced.exists(), "referenced entry survives despite age");
        assert!(garbage.exists(), "garbage stamp falls back to fresh mtime");

        // Idempotent: nothing left to collect.
        let again = store.gc(Duration::from_secs(3_600), false).unwrap();
        assert_eq!((again.entries_removed, again.freed), (0, 0));
    }

    #[test]
    fn gc_drops_refs_for_projects_that_no_longer_exist() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let sha = "e".repeat(64);
        let entry = seed_entry(&store, &sha, 7_200);

        // The referencing project directory disappears (deleted checkout):
        // its pin must not keep the entry alive.
        let project = tempfile::tempdir().unwrap();
        store
            .record_project(project.path(), vec![sha.clone()])
            .unwrap();
        drop(project);

        let report = store.gc(Duration::from_secs(3_600), false).unwrap();
        assert_eq!(report.entries_removed, 1);
        assert!(
            !entry.exists(),
            "entry pinned only by a dead project is pruned"
        );
    }

    #[test]
    fn human_size_formats_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1_023), "1023 B");
        assert_eq!(human_size(1_024), "1.0 KiB");
        assert_eq!(human_size(1_536), "1.5 KiB");
        assert_eq!(human_size(5 * 1_024 * 1_024), "5.0 MiB");
        assert_eq!(human_size(u64::MAX), "16777216.0 TiB");
    }
}
