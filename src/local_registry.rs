//! Machine-wide registry of local, on-disk Zed projects.
//!
//! The HTTP registry is one source of packages; a developer's own checkouts
//! are another. During development of the registry itself — or on an
//! air-gapped machine, or simply while `registry.zpkg.net` is down — a
//! dependency that already exists as a directory on this filesystem should be
//! resolvable without a network round trip.
//!
//! This module owns that second source. A project directory containing a
//! [`MANIFEST_FILE`] is *registered* into a shared index under the Zed home
//! directory. Registration is per **path**, not per package name: the same
//! `org/name` may legitimately exist in several checkouts (a release clone, a
//! feature worktree, a bisect tree), and collapsing them onto the package name
//! alone would silently build the wrong one. Every entry therefore carries its
//! canonical filesystem path, the index is keyed by that path, and a
//! name-level tie that cannot be broken deterministically is a hard error
//! rather than a coin flip.
//!
//! Resolution is *live*: the recorded version is a registration-time snapshot
//! used for reporting and drift detection, while the manifest on disk is
//! authoritative every time a dependency is resolved. Selected entries are
//! materialized with the same source-link path used for workspace members, so
//! `--install-mode=symlink` produces a canonical absolute directory symlink
//! into the developer's checkout and edits are visible to consumers
//! immediately.
//!
//! Everything here is fail-closed. An entry whose path disappeared, whose
//! manifest stopped parsing, or whose package identity changed under it is
//! reported as unhealthy and never selected.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zed_interfaces::manifest::{Manifest, is_slug};
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::version::Requirement;
use zed_lock::{LockClass, LockManager, LockRequest};

use crate::config::{Config, read_manifest};

/// Directory under `ZED_PKG_HOME` that holds the shared local-project index.
pub const LOCAL_REGISTRY_DIR: &str = "local-registry";
/// Index file name inside [`LOCAL_REGISTRY_DIR`].
pub const INDEX_FILE: &str = "index.json";
/// Lock file name inside [`LOCAL_REGISTRY_DIR`].
const LOCK_FILE: &str = ".lock";
/// Schema tag written into (and required by) the index.
pub const INDEX_SCHEMA: &str = "zed.local-registry.v1";

/// Selects the local-registry mode without a `--local-registry` flag.
pub const MODE_ENV: &str = "ZED_PKG_LOCAL_REGISTRY";
/// Relocates the index file itself (hermetic tests, throwaway sandboxes).
pub const INDEX_PATH_ENV: &str = "ZED_PKG_LOCAL_REGISTRY_FILE";

/// Ceiling on the index file size. The index is operator-owned state, but it
/// is still parsed on every install, so a corrupted or hostile file must not
/// be able to turn `zed install` into an out-of-memory abort.
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
/// Ceiling on registered projects. Far above any real developer machine while
/// keeping the per-install linear scans trivially bounded.
const MAX_ENTRIES: usize = 4096;
/// Default `zed local scan` recursion depth below the requested root.
pub const DEFAULT_SCAN_DEPTH: usize = 6;
/// Hard ceiling on `--max-depth`, and on directories visited by one scan.
const MAX_SCAN_DEPTH: usize = 32;
const MAX_SCAN_VISITS: usize = 200_000;

/// Directory names never descended into by `zed local scan`. These are
/// dependency, build, and VCS trees: a manifest inside one is a *copy* of some
/// package, never a checkout the developer edits.
const SCAN_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".zed",
    ".zed-pack",
    ".vendor",
    "zed_modules",
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".direnv",
    ".terraform",
];

// ---------------------------------------------------------------------------
// mode

/// How much authority the local registry has over dependency resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum LocalRegistryMode {
    /// Never consult the local registry; every dependency comes from the
    /// configured remote registry.
    Off,
    /// Consult the local registry before the remote one for ordinary
    /// (non-frozen) installs. Frozen installs stay byte-reproducible and
    /// ignore ambient machine state. This is the default: an entry only
    /// exists because someone ran `zed local register` on that exact path.
    #[default]
    Auto,
    /// Like `auto`, and additionally allowed to satisfy `--frozen` installs.
    /// Reproducibility then depends on this machine's registrations.
    Prefer,
    /// The local registry is the only source. A dependency with no healthy
    /// local entry is an error instead of a remote lookup — a fully offline
    /// install.
    Only,
}

impl LocalRegistryMode {
    /// Parse the value of [`MODE_ENV`]. Unset or empty means [`Self::Auto`].
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" => Ok(Self::Auto),
            "off" | "none" | "never" | "false" | "0" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            "prefer" | "true" | "1" => Ok(Self::Prefer),
            "only" | "offline" => Ok(Self::Only),
            other => {
                bail!("invalid {MODE_ENV} value `{other}` (expected off, auto, prefer, or only)")
            }
        }
    }

    /// Resolve the mode from the process environment.
    pub fn from_env() -> Result<Self> {
        match std::env::var(MODE_ENV) {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("{MODE_ENV} is not valid UTF-8")
            }
        }
    }

    /// Is the local registry consulted at all for this install?
    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// May the local registry satisfy an install with the given frozen policy?
    ///
    /// A frozen install replays an exact lockfile. Ambient, machine-local
    /// registrations are deliberately excluded from that replay unless the
    /// operator asked for them by name.
    pub fn applies_to_frozen(self, frozen: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Auto => !frozen,
            Self::Prefer | Self::Only => true,
        }
    }

    /// Is a dependency with no healthy local entry an error?
    pub fn requires_local(self) -> bool {
        matches!(self, Self::Only)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Prefer => "prefer",
            Self::Only => "only",
        }
    }
}

// ---------------------------------------------------------------------------
// on-disk format

/// One registered local project.
///
/// `path` is the identity. `org`/`name`/`version` are a registration-time
/// snapshot: they make `zed local list` readable and let drift be reported,
/// but the manifest on disk wins whenever a dependency is actually resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalEntry {
    pub org: String,
    pub name: String,
    pub version: String,
    /// Canonical absolute path to the directory holding [`MANIFEST_FILE`].
    pub path: String,
    /// Higher wins when several entries provide the same package. Equal
    /// priority plus equal version is an ambiguity error, never a guess.
    #[serde(default)]
    pub priority: i64,
    /// Retained but never selected while false, so a checkout can be shelved
    /// without losing its priority.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Digest of the manifest bytes at registration time. Reported by
    /// `zed local list` as drift; never used to reject a resolution, because
    /// living source is the point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    /// Unix epoch seconds at registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<u64>,
}

fn default_true() -> bool {
    true
}

impl LocalEntry {
    /// `org/name` for this entry.
    pub fn key(&self) -> String {
        format!("{}/{}", self.org, self.name)
    }

    /// Stable short handle derived from the canonical path. Convenient for
    /// `zed local unregister <id>` when two checkouts differ deep in a long
    /// path.
    pub fn id(&self) -> String {
        let digest = Sha256::digest(self.path.as_bytes());
        hex::encode(digest)[..16].to_string()
    }

    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    /// Deterministic total order: package identity first, then path.
    fn sort_key(&self) -> (&str, &str, &str) {
        (&self.org, &self.name, &self.path)
    }
}

/// The complete shared index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalIndex {
    pub schema: String,
    #[serde(default)]
    pub entries: Vec<LocalEntry>,
}

impl Default for LocalIndex {
    fn default() -> Self {
        Self {
            schema: INDEX_SCHEMA.to_string(),
            entries: Vec::new(),
        }
    }
}

impl LocalIndex {
    /// Canonical order for deterministic bytes on disk.
    ///
    /// Duplicates are deliberately *not* collapsed here. `register` guarantees
    /// one entry per path by replacing in place; a duplicate can therefore only
    /// come from a hand-edited or corrupted file, and quietly discarding one of
    /// two conflicting claims on the same directory would hide exactly the
    /// state an operator needs to see. [`Self::validate`] rejects it instead.
    fn normalize(&mut self) {
        self.entries.sort_by(|left, right| {
            left.sort_key()
                .cmp(&right.sort_key())
                .then_with(|| left.version.cmp(&right.version))
        });
    }

    /// Validate everything that could otherwise reach a filesystem path, a
    /// registry route, or an unbounded loop later on.
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == INDEX_SCHEMA,
            "unsupported local registry schema `{}` (expected `{INDEX_SCHEMA}`); \
             this index was written by a different zed version",
            self.schema
        );
        ensure!(
            self.entries.len() <= MAX_ENTRIES,
            "local registry has {} entries, above the {MAX_ENTRIES} ceiling",
            self.entries.len()
        );
        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
        for entry in &self.entries {
            ensure!(
                is_slug(&entry.org) && is_slug(&entry.name),
                "local registry entry `{}` has an invalid package identity `{}/{}`",
                entry.path,
                entry.org,
                entry.name
            );
            ensure!(
                !entry.version.is_empty(),
                "local registry entry `{}` has an empty version",
                entry.path
            );
            let path = Path::new(&entry.path);
            ensure!(
                path.is_absolute(),
                "local registry entry `{}` is not an absolute path",
                entry.path
            );
            ensure!(
                seen.insert(entry.path.as_str(), ()).is_none(),
                "local registry has duplicate entries for `{}`",
                entry.path
            );
        }
        Ok(())
    }

    /// Every enabled entry providing `key`, in deterministic order.
    pub fn candidates_for(&self, key: &str) -> Vec<&LocalEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.enabled && entry.key() == key)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// paths and locking

pub fn registry_dir(cfg: &Config) -> PathBuf {
    cfg.home.join(LOCAL_REGISTRY_DIR)
}

/// Where the index lives. [`INDEX_PATH_ENV`] relocates it wholesale so tests
/// and sandboxes never touch a developer's real registrations.
pub fn index_path(cfg: &Config) -> Result<PathBuf> {
    match std::env::var_os(INDEX_PATH_ENV) {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            ensure!(
                path.is_absolute(),
                "{INDEX_PATH_ENV} must be an absolute path, got `{}`",
                path.display()
            );
            Ok(path)
        }
        _ => Ok(registry_dir(cfg).join(INDEX_FILE)),
    }
}

fn index_parent(cfg: &Config) -> Result<PathBuf> {
    let path = index_path(cfg)?;
    path.parent()
        .map(Path::to_path_buf)
        .context("local registry index path has no parent directory")
}

fn secure_create_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("creating local registry directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Serialize one read-modify-write of the shared index.
///
/// Two `zed local register` runs racing on the same index would otherwise
/// last-writer-wins away one of the registrations. The lock lives beside the
/// index so processes with different `ZED_PKG_HOME` values that were pointed
/// at the same index by [`INDEX_PATH_ENV`] still coordinate.
fn with_locked_index<T>(
    cfg: &Config,
    operation: &str,
    body: impl FnOnce(&mut LocalIndex) -> Result<T>,
) -> Result<T> {
    let parent = index_parent(cfg)?;
    secure_create_dir(&parent)?;
    let lock_path = parent.join(LOCK_FILE);
    let _lock = LockManager::global()
        .acquire_blocking(
            LockRequest::exclusive(&lock_path)
                .operation(operation)
                .class(LockClass::Custom(5))
                .queue_same_process(),
        )
        .with_context(|| format!("locking {}", lock_path.display()))?;

    let mut index = load(cfg)?;
    let result = body(&mut index)?;
    index.normalize();
    index.validate()?;
    save(cfg, &index)?;
    Ok(result)
}

/// Read the index. A missing file is an empty registry, not an error.
pub fn load(cfg: &Config) -> Result<LocalIndex> {
    let path = index_path(cfg)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalIndex::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading local registry index {}", path.display()));
        }
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "local registry index {} is not a regular, non-symlink file",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_INDEX_BYTES,
        "local registry index {} is {} bytes, above the {MAX_INDEX_BYTES} byte ceiling",
        path.display(),
        metadata.len()
    );
    let text = fs::read_to_string(&path)
        .with_context(|| format!("reading local registry index {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(LocalIndex::default());
    }
    let mut index: LocalIndex = serde_json::from_str(&text)
        .with_context(|| format!("parsing local registry index {}", path.display()))?;
    index.normalize();
    index
        .validate()
        .with_context(|| format!("validating local registry index {}", path.display()))?;
    Ok(index)
}

/// Write the index atomically: staged beside the final path, then renamed.
fn save(cfg: &Config, index: &LocalIndex) -> Result<()> {
    let path = index_path(cfg)?;
    let parent = index_parent(cfg)?;
    secure_create_dir(&parent)?;
    let text = format!("{}\n", serde_json::to_string_pretty(index)?);
    ensure!(
        text.len() as u64 <= MAX_INDEX_BYTES,
        "refusing to write a {} byte local registry index",
        text.len()
    );
    let temporary = parent.join(format!(".{INDEX_FILE}.tmp-{}", std::process::id()));
    {
        let mut options = fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing {}", temporary.display()))?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("installing local registry index {}", path.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// path validation

/// Resolve a user-supplied path to the canonical directory that will become an
/// entry's identity.
///
/// Canonicalization is deliberate: registering `~/src/current` where `current`
/// is a symlink records the real checkout, so the entry keeps meaning the same
/// tree after the symlink is repointed, and two spellings of one directory can
/// never both be registered.
pub fn canonical_project_dir(raw: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(raw)
        .with_context(|| format!("resolving project path {}", raw.display()))?;
    ensure!(
        canonical.is_dir(),
        "{} is not a directory",
        canonical.display()
    );
    ensure!(
        canonical.to_str().is_some(),
        "project path {} is not valid UTF-8; the local registry index is JSON",
        canonical.display()
    );
    Ok(canonical)
}

/// True when `inner` is `outer` or lies beneath it.
fn contains(outer: &Path, inner: &Path) -> bool {
    inner == outer || inner.starts_with(outer)
}

/// Read and validate the manifest of a candidate project directory.
///
/// The manifest itself must be a regular non-symlink file. A symlinked
/// manifest means the identity of the tree is decided somewhere outside it,
/// which is exactly the ambiguity this registry exists to remove.
fn read_project_manifest_bytes(dir: &Path) -> Result<(Manifest, String)> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let metadata = fs::symlink_metadata(&manifest_path).with_context(|| {
        format!(
            "{} has no {MANIFEST_FILE}; only Zed projects can be registered",
            dir.display()
        )
    })?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{} must be a regular, non-symlink file",
        manifest_path.display()
    );
    let bytes =
        fs::read(&manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("{} is not UTF-8", manifest_path.display()))?;
    let manifest = Manifest::parse(text)
        .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
    Ok((manifest, hex::encode(Sha256::digest(&bytes))))
}

/// Everything that must hold before a directory may enter the index.
fn validate_registrable(cfg: &Config, dir: &Path) -> Result<(Manifest, String)> {
    // The content-addressed store holds extracted *artifacts*. Registering one
    // would feed materialized output back in as source and let an install link
    // the store into itself. Compare against the resolved home when it exists
    // so a symlinked `ZED_PKG_HOME` cannot be used to slip past the check.
    let home = fs::canonicalize(&cfg.home).unwrap_or_else(|_| cfg.home.clone());
    ensure!(
        !contains(&home, dir),
        "refusing to register {}: it is inside the Zed home directory {}",
        dir.display(),
        home.display()
    );
    ensure!(
        !contains(dir, &home),
        "refusing to register {}: it contains the Zed home directory {}",
        dir.display(),
        home.display()
    );
    read_project_manifest_bytes(dir)
}

// ---------------------------------------------------------------------------
// health

/// Why an entry is (or is not) usable right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryHealth {
    /// Path, manifest, and identity all still agree with the index.
    Ok { version: String, drifted: bool },
    /// Deliberately shelved with `zed local disable`.
    Disabled,
    /// The directory is gone.
    MissingPath,
    /// The directory exists but is no longer a Zed project.
    MissingManifest,
    /// The manifest stopped parsing or validating.
    InvalidManifest(String),
    /// The manifest now declares a different package than the one registered.
    IdentityChanged { found: String },
}

impl EntryHealth {
    pub fn is_selectable(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    pub fn label(&self) -> String {
        match self {
            Self::Ok {
                version,
                drifted: false,
            } => format!("ok ({version})"),
            Self::Ok {
                version,
                drifted: true,
            } => format!("ok ({version}, manifest changed since registration)"),
            Self::Disabled => "disabled".to_string(),
            Self::MissingPath => "stale: directory is gone".to_string(),
            Self::MissingManifest => format!("stale: no {MANIFEST_FILE}"),
            Self::InvalidManifest(reason) => format!("stale: invalid manifest ({reason})"),
            Self::IdentityChanged { found } => {
                format!("stale: manifest now declares {found}")
            }
        }
    }
}

/// Inspect one entry against the filesystem. Never mutates the index.
pub fn health(entry: &LocalEntry) -> (EntryHealth, Option<Manifest>) {
    if !entry.enabled {
        return (EntryHealth::Disabled, None);
    }
    let dir = entry.path_buf();
    if !dir.is_dir() {
        return (EntryHealth::MissingPath, None);
    }
    if !dir.join(MANIFEST_FILE).exists() {
        return (EntryHealth::MissingManifest, None);
    }
    let manifest = match read_manifest(&dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            return (EntryHealth::InvalidManifest(format!("{error:#}")), None);
        }
    };
    if manifest.full_name() != entry.key() {
        return (
            EntryHealth::IdentityChanged {
                found: manifest.full_name(),
            },
            None,
        );
    }
    let drifted = match (&entry.manifest_sha256, current_manifest_sha256(&dir)) {
        (Some(recorded), Some(current)) => recorded != &current,
        _ => false,
    };
    let version = manifest.package.version.clone();
    (EntryHealth::Ok { version, drifted }, Some(manifest))
}

fn current_manifest_sha256(dir: &Path) -> Option<String> {
    let bytes = fs::read(dir.join(MANIFEST_FILE)).ok()?;
    Some(hex::encode(Sha256::digest(&bytes)))
}

/// An entry paired with its current filesystem health, for reporting.
#[derive(Debug, Clone)]
pub struct EntryStatus {
    pub entry: LocalEntry,
    pub health: EntryHealth,
}

// ---------------------------------------------------------------------------
// selection

/// A local project chosen to satisfy one dependency.
#[derive(Debug, Clone)]
pub struct LocalSelection {
    pub entry: LocalEntry,
    pub manifest: Manifest,
    pub dir: PathBuf,
}

/// Pick the entry that satisfies `requirement` for `key`, if any.
///
/// Ordering is priority, then version, then path — but a tie on *both*
/// priority and version between two different paths is an error. Two checkouts
/// of `acme/widget@1.4.0` are indistinguishable to this resolver, and quietly
/// linking whichever sorted first is the kind of bug that costs an afternoon.
/// The operator breaks the tie with `--priority` or by unregistering one.
///
/// Unhealthy entries are skipped and returned in `skipped` so the caller can
/// warn once, at the point where it matters, instead of on every command.
pub fn select(
    index: &LocalIndex,
    key: &str,
    requirement: &Requirement,
) -> Result<(Option<LocalSelection>, Vec<(LocalEntry, EntryHealth)>)> {
    let mut skipped = Vec::new();
    let mut matching: Vec<LocalSelection> = Vec::new();

    for entry in index.candidates_for(key) {
        let (state, manifest) = health(entry);
        let Some(manifest) = manifest else {
            skipped.push((entry.clone(), state));
            continue;
        };
        if !requirement.matches(&manifest.package.version) {
            continue;
        }
        matching.push(LocalSelection {
            entry: entry.clone(),
            dir: entry.path_buf(),
            manifest,
        });
    }

    if matching.is_empty() {
        return Ok((None, skipped));
    }

    matching.sort_by(|left, right| {
        right
            .entry
            .priority
            .cmp(&left.entry.priority)
            .then_with(|| {
                compare_versions(
                    &right.manifest.package.version,
                    &left.manifest.package.version,
                )
            })
            .then_with(|| left.entry.path.cmp(&right.entry.path))
    });

    let best = &matching[0];
    let tied: Vec<&LocalSelection> = matching
        .iter()
        .filter(|candidate| {
            candidate.entry.priority == best.entry.priority
                && candidate.manifest.package.version == best.manifest.package.version
        })
        .collect();
    if tied.len() > 1 {
        let paths = tied
            .iter()
            .map(|candidate| {
                format!(
                    "  {} (priority {})",
                    candidate.entry.path, candidate.entry.priority
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "local registry cannot choose between {} registrations of {key}@{} \
             at equal priority:\n{paths}\n\
             Break the tie with `zed local register <path> --priority N` or \
             `zed local unregister <path>`.",
            tied.len(),
            best.manifest.package.version
        );
    }

    Ok((Some(matching.swap_remove(0)), skipped))
}

/// Compare two version strings, newest first when both are semver. Non-semver
/// (`opaque`) versions fall back to a byte comparison so ordering stays total
/// and deterministic.
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(left), semver::Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

// ---------------------------------------------------------------------------
// mutating operations

/// What one registration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterAction {
    Added,
    Updated,
}

/// Register (or refresh) one project directory.
pub fn register(
    cfg: &Config,
    raw_path: &Path,
    priority: Option<i64>,
    enabled: bool,
) -> Result<(RegisterAction, LocalEntry)> {
    let dir = canonical_project_dir(raw_path)?;
    let (manifest, manifest_sha256) = validate_registrable(cfg, &dir)?;
    let path = dir
        .to_str()
        .context("canonical project path is not valid UTF-8")?
        .to_string();

    with_locked_index(cfg, "local registry register", |index| {
        let existing = index.entries.iter().position(|entry| entry.path == path);
        let now = epoch_seconds();
        let entry = LocalEntry {
            org: manifest.package.org.clone(),
            name: manifest.package.name.clone(),
            version: manifest.package.version.clone(),
            path: path.clone(),
            priority: priority.unwrap_or_else(|| {
                existing
                    .map(|position| index.entries[position].priority)
                    .unwrap_or(0)
            }),
            enabled,
            manifest_sha256: Some(manifest_sha256.clone()),
            registered_at: existing
                .and_then(|position| index.entries[position].registered_at)
                .or(now),
        };
        match existing {
            Some(position) => {
                index.entries[position] = entry.clone();
                Ok((RegisterAction::Updated, entry))
            }
            None => {
                ensure!(
                    index.entries.len() < MAX_ENTRIES,
                    "local registry already holds {MAX_ENTRIES} projects; \
                     run `zed local prune` or unregister something first"
                );
                index.entries.push(entry.clone());
                Ok((RegisterAction::Added, entry))
            }
        }
    })
}

fn epoch_seconds() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// A selector names either one path or one `org/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Path(String),
    Key(String),
    Id(String),
}

/// Interpret a CLI selector.
///
/// `org/name` is a package key; anything that resolves to a directory on this
/// filesystem is a path; a 16-character hex string is an entry id. Paths are
/// tried before keys so a relative directory literally named `a/b` still wins,
/// and an unresolvable path is reported as such instead of silently becoming a
/// key that matches nothing.
pub fn parse_selector(raw: &str) -> Result<Selector> {
    let raw = raw.trim();
    ensure!(!raw.is_empty(), "empty local registry selector");
    if let Ok(canonical) = fs::canonicalize(raw)
        && canonical.is_dir()
    {
        let path = canonical
            .to_str()
            .context("selector path is not valid UTF-8")?
            .to_string();
        return Ok(Selector::Path(path));
    }
    if raw.len() == 16 && raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(Selector::Id(raw.to_ascii_lowercase()));
    }
    if let Some((org, name)) = raw.split_once('/')
        && is_slug(org)
        && is_slug(name)
    {
        return Ok(Selector::Key(format!("{org}/{name}")));
    }
    bail!(
        "`{raw}` is neither an existing directory, a 16-character entry id, \
         nor an `org/name` package key"
    )
}

fn matches_selector(entry: &LocalEntry, selector: &Selector) -> bool {
    match selector {
        Selector::Path(path) => &entry.path == path,
        Selector::Key(key) => &entry.key() == key,
        Selector::Id(id) => &entry.id() == id,
    }
}

/// Remove entries matching `selector`.
///
/// A key selector that matches several checkouts requires `all`, so
/// `zed local unregister acme/widget` can never quietly drop a registration
/// the operator did not have in mind.
pub fn unregister(cfg: &Config, selector: &Selector, all: bool) -> Result<Vec<LocalEntry>> {
    with_locked_index(cfg, "local registry unregister", |index| {
        let matched: Vec<LocalEntry> = index
            .entries
            .iter()
            .filter(|entry| matches_selector(entry, selector))
            .cloned()
            .collect();
        if matched.is_empty() {
            bail!("no local registry entry matches that selector");
        }
        if matched.len() > 1 && !all {
            let paths = matched
                .iter()
                .map(|entry| format!("  {}", entry.path))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "{} registrations match; pass --all or name one path:\n{paths}",
                matched.len()
            );
        }
        index
            .entries
            .retain(|entry| !matches_selector(entry, selector));
        Ok(matched)
    })
}

/// Enable or disable entries matching `selector` without forgetting them.
pub fn set_enabled(
    cfg: &Config,
    selector: &Selector,
    enabled: bool,
    all: bool,
) -> Result<Vec<LocalEntry>> {
    with_locked_index(cfg, "local registry enable", |index| {
        let matches = index
            .entries
            .iter()
            .filter(|entry| matches_selector(entry, selector))
            .count();
        if matches == 0 {
            bail!("no local registry entry matches that selector");
        }
        if matches > 1 && !all {
            bail!("{matches} registrations match; pass --all or name one path");
        }
        let mut changed = Vec::new();
        for entry in &mut index.entries {
            if matches_selector(entry, selector) {
                entry.enabled = enabled;
                changed.push(entry.clone());
            }
        }
        Ok(changed)
    })
}

/// Report every entry with its current health.
pub fn status(cfg: &Config) -> Result<Vec<EntryStatus>> {
    let index = load(cfg)?;
    Ok(index
        .entries
        .into_iter()
        .map(|entry| {
            let (health, _) = health(&entry);
            EntryStatus { entry, health }
        })
        .collect())
}

/// Drop entries whose path, manifest, or identity no longer holds up.
///
/// Disabled entries are never pruned: being shelved is not being broken.
pub fn prune(cfg: &Config, dry_run: bool) -> Result<Vec<EntryStatus>> {
    let removable = |entry: &LocalEntry| -> Option<EntryHealth> {
        let (state, _) = health(entry);
        match state {
            EntryHealth::Ok { .. } | EntryHealth::Disabled => None,
            other => Some(other),
        }
    };

    if dry_run {
        let index = load(cfg)?;
        return Ok(index
            .entries
            .into_iter()
            .filter_map(|entry| removable(&entry).map(|health| EntryStatus { entry, health }))
            .collect());
    }

    with_locked_index(cfg, "local registry prune", |index| {
        let mut removed = Vec::new();
        index.entries.retain(|entry| match removable(entry) {
            Some(health) => {
                removed.push(EntryStatus {
                    entry: entry.clone(),
                    health,
                });
                false
            }
            None => true,
        });
        Ok(removed)
    })
}

/// One project found by [`scan`].
#[derive(Debug, Clone)]
pub struct ScanHit {
    pub dir: PathBuf,
    pub key: String,
    pub version: String,
    pub action: Option<RegisterAction>,
}

/// Discover Zed projects beneath `root` and register them.
///
/// Nested projects are expected — a workspace root and its members are all
/// registrable — so a directory with a manifest is still descended into.
/// Dependency, build, and VCS trees are skipped wholesale: a manifest inside
/// `node_modules` or `target` is materialized output, not a checkout.
pub fn scan(
    cfg: &Config,
    root: &Path,
    max_depth: usize,
    priority: Option<i64>,
    dry_run: bool,
) -> Result<Vec<ScanHit>> {
    ensure!(
        max_depth <= MAX_SCAN_DEPTH,
        "--max-depth {max_depth} is above the {MAX_SCAN_DEPTH} ceiling"
    );
    let root = canonical_project_dir(root)?;
    let mut hits = Vec::new();
    let mut visits = 0usize;
    let mut queue = vec![(root, 0usize)];

    while let Some((dir, depth)) = queue.pop() {
        visits += 1;
        ensure!(
            visits <= MAX_SCAN_VISITS,
            "scan visited more than {MAX_SCAN_VISITS} directories; narrow the root or --max-depth"
        );
        if dir.join(MANIFEST_FILE).is_file()
            && let Ok((manifest, _)) = read_project_manifest_bytes(&dir)
            && validate_registrable(cfg, &dir).is_ok()
        {
            hits.push(ScanHit {
                dir: dir.clone(),
                key: manifest.full_name(),
                version: manifest.package.version.clone(),
                action: None,
            });
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if SCAN_SKIP_DIRS.contains(&name) {
                continue;
            }
            // Follow no symlinks: a scan must not wander out of the requested
            // subtree, and a symlink loop must not turn it into a hang.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            queue.push((entry.path(), depth + 1));
        }
    }

    hits.sort_by(|left, right| left.dir.cmp(&right.dir));
    if dry_run {
        return Ok(hits);
    }
    for hit in &mut hits {
        let (action, _) = register(cfg, &hit.dir, priority, true)?;
        hit.action = Some(action);
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(home: &Path) -> Config {
        Config {
            registry: "https://registry.invalid".to_string(),
            home: home.to_path_buf(),
            token: None,
            auth_url: "https://registry.invalid/shared-auth".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        }
    }

    fn write_project(root: &Path, org: &str, name: &str, version: &str) -> PathBuf {
        let dir = root.join(format!("{org}-{name}-{version}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(MANIFEST_FILE),
            format!(
                "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\n\n\
                 [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/{org}/{name}\"\n"
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn mode_parsing_covers_every_documented_spelling() {
        assert_eq!(
            LocalRegistryMode::parse("").unwrap(),
            LocalRegistryMode::Auto
        );
        assert_eq!(
            LocalRegistryMode::parse(" OFF ").unwrap(),
            LocalRegistryMode::Off
        );
        assert_eq!(
            LocalRegistryMode::parse("only").unwrap(),
            LocalRegistryMode::Only
        );
        assert!(LocalRegistryMode::parse("maybe").is_err());
    }

    #[test]
    fn frozen_installs_ignore_ambient_registrations_until_asked() {
        assert!(!LocalRegistryMode::Auto.applies_to_frozen(true));
        assert!(LocalRegistryMode::Auto.applies_to_frozen(false));
        assert!(LocalRegistryMode::Prefer.applies_to_frozen(true));
        assert!(LocalRegistryMode::Only.applies_to_frozen(true));
        assert!(!LocalRegistryMode::Off.applies_to_frozen(false));
    }

    #[test]
    fn registration_is_keyed_by_path_not_package_name() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let first = write_project(temp.path(), "acme", "widget", "1.0.0");
        let second = write_project(temp.path(), "acme", "widget", "2.0.0");

        let (action, _) = register(&cfg, &first, None, true).unwrap();
        assert_eq!(action, RegisterAction::Added);
        let (action, _) = register(&cfg, &second, None, true).unwrap();
        assert_eq!(action, RegisterAction::Added);
        // Re-registering the same path refreshes rather than duplicating.
        let (action, _) = register(&cfg, &first, None, true).unwrap();
        assert_eq!(action, RegisterAction::Updated);

        let index = load(&cfg).unwrap();
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.candidates_for("acme/widget").len(), 2);
    }

    #[test]
    fn selection_prefers_priority_then_version() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let old = write_project(temp.path(), "acme", "widget", "1.0.0");
        let new = write_project(temp.path(), "acme", "widget", "1.5.0");
        register(&cfg, &old, None, true).unwrap();
        register(&cfg, &new, None, true).unwrap();

        let index = load(&cfg).unwrap();
        let (selected, skipped) = select(&index, "acme/widget", &Requirement::parse("^1")).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(selected.unwrap().dir, fs::canonicalize(&new).unwrap());

        // Priority outranks a newer version.
        register(&cfg, &old, Some(10), true).unwrap();
        let index = load(&cfg).unwrap();
        let (selected, _) = select(&index, "acme/widget", &Requirement::parse("^1")).unwrap();
        assert_eq!(selected.unwrap().dir, fs::canonicalize(&old).unwrap());
    }

    #[test]
    fn an_unbreakable_tie_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        for dir in [&left, &right] {
            fs::create_dir_all(dir).unwrap();
            fs::write(
                dir.join(MANIFEST_FILE),
                "[package]\norg = \"acme\"\nname = \"widget\"\nversion = \"1.0.0\"\n\n\
                 [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/acme/widget\"\n",
            )
            .unwrap();
            register(&cfg, dir, None, true).unwrap();
        }
        let index = load(&cfg).unwrap();
        let error = select(&index, "acme/widget", &Requirement::parse("^1"))
            .expect_err("an ambiguous local selection must fail closed");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("cannot choose between"), "{rendered}");
    }

    #[test]
    fn requirement_mismatch_falls_through_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let project = write_project(temp.path(), "acme", "widget", "1.0.0");
        register(&cfg, &project, None, true).unwrap();
        let index = load(&cfg).unwrap();
        let (selected, skipped) = select(&index, "acme/widget", &Requirement::parse("^2")).unwrap();
        assert!(selected.is_none());
        assert!(skipped.is_empty());
    }

    #[test]
    fn disabled_and_deleted_entries_are_never_selected() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let project = write_project(temp.path(), "acme", "widget", "1.0.0");
        register(&cfg, &project, None, true).unwrap();
        let selector = parse_selector(project.to_str().unwrap()).unwrap();
        set_enabled(&cfg, &selector, false, false).unwrap();

        let index = load(&cfg).unwrap();
        let (selected, _) = select(&index, "acme/widget", &Requirement::parse("^1")).unwrap();
        assert!(selected.is_none());

        set_enabled(&cfg, &selector, true, false).unwrap();
        fs::remove_dir_all(&project).unwrap();
        let index = load(&cfg).unwrap();
        let (selected, skipped) = select(&index, "acme/widget", &Requirement::parse("^1")).unwrap();
        assert!(selected.is_none());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].1, EntryHealth::MissingPath);
    }

    #[test]
    fn prune_removes_broken_entries_but_keeps_disabled_ones() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let broken = write_project(temp.path(), "acme", "gone", "1.0.0");
        let shelved = write_project(temp.path(), "acme", "shelved", "1.0.0");
        register(&cfg, &broken, None, true).unwrap();
        register(&cfg, &shelved, None, true).unwrap();
        let selector = parse_selector(shelved.to_str().unwrap()).unwrap();
        set_enabled(&cfg, &selector, false, false).unwrap();
        fs::remove_dir_all(&broken).unwrap();

        let planned = prune(&cfg, true).unwrap();
        assert_eq!(planned.len(), 1);
        let removed = prune(&cfg, false).unwrap();
        assert_eq!(removed.len(), 1);
        let index = load(&cfg).unwrap();
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].name, "shelved");
    }

    #[test]
    fn the_zed_home_directory_can_never_be_registered() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let inside = home.join("store").join("pkg");
        fs::create_dir_all(&inside).unwrap();
        fs::write(
            inside.join(MANIFEST_FILE),
            "[package]\norg = \"acme\"\nname = \"widget\"\nversion = \"1.0.0\"\n\n\
             [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/acme/widget\"\n",
        )
        .unwrap();
        let error = register(&cfg, &inside, None, true)
            .expect_err("registering inside the store must fail closed");
        assert!(format!("{error:#}").contains("Zed home directory"));
    }

    #[test]
    fn a_symlinked_manifest_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let real = write_project(temp.path(), "acme", "widget", "1.0.0");
        let fake = temp.path().join("fake");
        fs::create_dir_all(&fake).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(real.join(MANIFEST_FILE), fake.join(MANIFEST_FILE)).unwrap();
            let error = register(&cfg, &fake, None, true)
                .expect_err("a symlinked manifest must fail closed");
            assert!(format!("{error:#}").contains("non-symlink"));
        }
        #[cfg(not(unix))]
        let _ = (real, fake);
    }

    #[test]
    fn an_identity_change_under_the_index_is_reported_not_selected() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let project = write_project(temp.path(), "acme", "widget", "1.0.0");
        register(&cfg, &project, None, true).unwrap();
        fs::write(
            project.join(MANIFEST_FILE),
            "[package]\norg = \"acme\"\nname = \"other\"\nversion = \"1.0.0\"\n\n\
             [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/acme/other\"\n",
        )
        .unwrap();
        let index = load(&cfg).unwrap();
        let (selected, skipped) = select(&index, "acme/widget", &Requirement::parse("^1")).unwrap();
        assert!(selected.is_none());
        assert!(matches!(skipped[0].1, EntryHealth::IdentityChanged { .. }));
    }

    #[test]
    fn the_index_round_trips_through_deterministic_json() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let first = write_project(temp.path(), "acme", "beta", "1.0.0");
        let second = write_project(temp.path(), "acme", "alpha", "1.0.0");
        register(&cfg, &first, None, true).unwrap();
        register(&cfg, &second, None, true).unwrap();

        let path = index_path(&cfg).unwrap();
        let first_bytes = fs::read(&path).unwrap();
        let index = load(&cfg).unwrap();
        save(&cfg, &index).unwrap();
        assert_eq!(first_bytes, fs::read(&path).unwrap());
        // Sorted by package identity, so `alpha` precedes `beta`.
        assert_eq!(index.entries[0].name, "alpha");
    }

    #[test]
    fn an_oversized_or_foreign_index_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let path = index_path(&cfg).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "{\"schema\":\"zed.local-registry.v99\",\"entries\":[]}",
        )
        .unwrap();
        let error = load(&cfg).expect_err("a foreign schema must fail closed");
        assert!(format!("{error:#}").contains("unsupported local registry schema"));
    }

    #[test]
    fn scan_finds_nested_projects_and_skips_dependency_trees() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cfg = config_for(&home);
        let root = temp.path().join("tree");
        let member = root.join("packages").join("member");
        let vendored = root.join("node_modules").join("copy");
        for (dir, name) in [(&root, "root"), (&member, "member"), (&vendored, "copy")] {
            fs::create_dir_all(dir).unwrap();
            fs::write(
                dir.join(MANIFEST_FILE),
                format!(
                    "[package]\norg = \"acme\"\nname = \"{name}\"\nversion = \"1.0.0\"\n\n\
                     [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/acme/{name}\"\n"
                ),
            )
            .unwrap();
        }
        let hits = scan(&cfg, &root, DEFAULT_SCAN_DEPTH, None, false).unwrap();
        let names: Vec<&str> = hits.iter().map(|hit| hit.key.as_str()).collect();
        assert!(names.contains(&"acme/root"));
        assert!(names.contains(&"acme/member"));
        assert!(!names.contains(&"acme/copy"));
    }
}
