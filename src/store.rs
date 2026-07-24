use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use zed_interfaces::paths::{ARCHIVE_ROOT, STORE_PKG_DIR, store_entry_rel};

use crate::pack::sha256_file;

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
    fn acquire(path: &Path) -> Result<Self> {
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
        file.lock_exclusive()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { _file: file })
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

    /// Root of the per-target build cache (zed-docs issue #5). Kept separate
    /// from the source store so the source store stays platform-independent
    /// and shareable, while compiled output is keyed by target.
    pub fn build_root(&self) -> PathBuf {
        self.home.join("build")
    }

    /// Build-cache entry for a `(target triple, key)` pair, where `key`
    /// folds in the source sha256 and the build command.
    pub fn build_entry(&self, triple: &str, key: &str) -> PathBuf {
        self.build_root().join(triple).join(key)
    }

    pub fn build_pkg_dir(&self, triple: &str, key: &str) -> PathBuf {
        self.build_entry(triple, key).join(STORE_PKG_DIR)
    }

    pub fn has_build(&self, triple: &str, key: &str) -> bool {
        self.build_pkg_dir(triple, key).is_dir()
    }

    /// Serialize concurrent builds of the same key (only one process builds).
    pub fn build_lock(&self, key: &str) -> Result<ProcessLock> {
        ProcessLock::acquire(&self.locks_dir().join(format!("build-{key}.lock")))
    }

    pub fn build_size(&self) -> u64 {
        dir_size(&self.build_root())
    }

    /// Serializes the whole install (refs.json + lockfile writes) against
    /// other zed processes. Held by the caller for the duration of install.
    pub fn install_lock(&self) -> Result<ProcessLock> {
        ProcessLock::acquire(&self.locks_dir().join("install.lock"))
    }

    /// Verify the archive hash and extract it into the store. Idempotent and
    /// safe under concurrency: a per-sha flock means only one process
    /// extracts a given artifact, and extraction still goes via a temp dir
    /// and atomic rename as a second line of defense.
    pub fn add_artifact(&self, archive: &Path, expected_sha256: &str) -> Result<PathBuf> {
        let (actual, _) = sha256_file(archive)?;
        if actual != expected_sha256 {
            bail!(
                "artifact hash mismatch: expected {expected_sha256}, got {actual} ({})",
                archive.display()
            );
        }
        let entry = self.entry_dir(expected_sha256);
        if self.has(expected_sha256) {
            return Ok(entry.join(STORE_PKG_DIR));
        }
        // Only one process extracts this sha at a time; the rest wait here
        // and then see has()==true.
        let _lock =
            ProcessLock::acquire(&self.locks_dir().join(format!("{expected_sha256}.lock")))?;
        if self.has(expected_sha256) {
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
        Ok(entry.join(STORE_PKG_DIR))
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

    /// Least-recently-used garbage collection (zed-docs issue #7): drop source
    /// store entries, per-target build-cache entries, and cached downloads not
    /// accessed within `max_age`. Complements ref-based [`Store::prune`] —
    /// everything removed is content-addressed and re-fetchable on next
    /// install. `dry_run` reports what would go without deleting.
    pub fn gc(&self, max_age: Duration, dry_run: bool) -> Result<GcReport> {
        let cutoff = SystemTime::now().checked_sub(max_age).unwrap_or(UNIX_EPOCH);
        let mut removed = 0usize;
        let mut freed = 0u64;

        let mut sweep_dir_of_entries = |dir: &Path| -> Result<()> {
            for shard in read_children(dir) {
                for entry in read_children(&shard) {
                    if entry.is_dir() && last_access(&entry) < cutoff {
                        freed += dir_size(&entry);
                        if !dry_run {
                            fs::remove_dir_all(&entry)?;
                        }
                        removed += 1;
                    }
                }
            }
            Ok(())
        };
        // Source store: store/v1/<shard>/<sha>.
        sweep_dir_of_entries(&self.root().join(zed_interfaces::paths::STORE_VERSION))?;
        // Build cache: build/<triple>/<key>.
        sweep_dir_of_entries(&self.build_root())?;

        // Download cache: cache/<sha>.tar.gz (flat files).
        for file in read_children(&self.cache_dir()) {
            if file.is_file() && last_access(&file) < cutoff {
                freed += file.metadata().map(|m| m.len()).unwrap_or(0);
                if !dry_run {
                    let _ = fs::remove_file(&file);
                }
                removed += 1;
            }
        }
        Ok(GcReport {
            removed,
            freed,
            dry_run,
        })
    }
}

/// Result of a [`Store::gc`] sweep.
#[derive(Debug, Clone, Copy)]
pub struct GcReport {
    pub removed: usize,
    pub freed: u64,
    pub dry_run: bool,
}

fn read_children(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// The most recent access time within `path` (file or directory tree),
/// falling back to modification time on filesystems that don't track atime
/// (e.g. `noatime`/`relatime`), so a stale entry is still reaped by age.
fn last_access(path: &Path) -> SystemTime {
    let mut newest = UNIX_EPOCH;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if let Ok(md) = entry.metadata() {
            let t = md
                .accessed()
                .or_else(|_| md.modified())
                .unwrap_or(UNIX_EPOCH);
            if t > newest {
                newest = t;
            }
        }
    }
    newest
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
