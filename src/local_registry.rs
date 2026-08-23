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
use std::path::{Component, Path, PathBuf};
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
    /// Where this checkout lives, recorded at registration. Drives the
    /// unavailable-versus-missing distinction and the link decision.
    #[serde(default)]
    pub volume: VolumeInfo,
    /// This entry's preference for how it reaches `zed_modules/`. A process
    /// wide `--local-link-policy` overrides it.
    #[serde(default)]
    pub link_policy: LinkPolicy,
    /// Set when a path map rewrote this entry on load: the form the index
    /// actually holds, so a save maps it back and a report can show both.
    #[serde(skip)]
    pub stored_path: Option<String>,
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
// portability: volumes, containers, and how a checkout reaches zed_modules/
//
// A registered path routinely lives somewhere less permanent than `$HOME`: an
// external or virtual disk that is mounted today and gone tomorrow, a
// container bind mount where the same bytes have a different absolute path on
// each side, or a build mount that exists for one `RUN` step and is absent
// from the resulting image. Three mechanisms cover those, and all three are
// pure filesystem logic — nothing here opens a socket:
//
// 1. [`PathMap`] rewrites a registered path between host and container views,
//    so one index file can be shared through the very bind mount it describes.
// 2. [`VolumeKind`] records where an entry lives, so a lookup can say
//    "temporarily unavailable" instead of "deleted" when a disk is unplugged.
// 3. [`LinkPolicy`] refuses to leave a symlink pointing at media that will not
//    be there later: removable disks and build mounts are copied, never
//    linked, so the installed tree survives the mount going away.

/// `host=container` path rewrites, separated by `,` (or `;`).
pub const PATH_MAP_ENV: &str = "ZED_PKG_LOCAL_REGISTRY_PATH_MAP";
/// Force a link policy (`auto`, `symlink`, `copy`).
pub const LINK_POLICY_ENV: &str = "ZED_PKG_LOCAL_LINK_POLICY";
/// Treat every registration as living on media that will not outlive this
/// process, so nothing is symlinked. Container image builds set this.
pub const EPHEMERAL_ENV: &str = "ZED_PKG_LOCAL_REGISTRY_EPHEMERAL";

/// Where a registered checkout physically lives.
///
/// Advisory metadata: it steers link policy and produces a good diagnostic, and
/// a wrong guess degrades to the conservative choice (copy) rather than to a
/// wrong install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeKind {
    /// An ordinary internal disk: present whenever the machine is.
    Fixed,
    /// A removable or virtual disk (USB, external SSD, mounted disk image,
    /// `/Volumes/*` on macOS, `/media|/mnt/*` on Linux). May vanish.
    Removable,
    /// A network filesystem (NFS, SMB, sshfs, 9p). May become unreachable.
    Network,
    /// A container bind mount or volume: the same bytes are reachable under a
    /// different path on the host, and may not exist in a later image layer.
    ContainerMount,
    /// Could not be determined.
    #[default]
    Unknown,
}

impl VolumeKind {
    /// Media that may disappear between this install and the next process that
    /// walks `zed_modules/`. A symlink into such media is a latent breakage.
    pub fn is_ephemeral(self) -> bool {
        matches!(self, Self::Removable | Self::Network | Self::ContainerMount)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Removable => "removable",
            Self::Network => "network",
            Self::ContainerMount => "container-mount",
            Self::Unknown => "unknown",
        }
    }
}

/// Recorded provenance of the volume an entry lives on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    #[serde(default)]
    pub kind: VolumeKind,
    /// Longest mount point that is a prefix of the entry path, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_point: Option<String>,
    /// Filesystem type reported by the mount table, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_type: Option<String>,
}

/// Are we executing inside a container? Used only to pick better defaults and
/// to explain a lookup; never to change what a path means.
pub fn in_container() -> bool {
    if std::env::var_os("ZED_PKG_IN_CONTAINER").is_some() {
        return true;
    }
    if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        return true;
    }
    fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroups| {
        cgroups.lines().any(|line| {
            line.contains("docker") || line.contains("containerd") || line.contains("kubepods")
        })
    })
}

#[derive(Debug, Clone)]
struct MountEntry {
    mount_point: PathBuf,
    fs_type: String,
    source: String,
}

/// `/proc/self/mountinfo` is the only table parsed: it is present on Linux
/// (including every container runtime that matters here) and its shape is
/// stable. Elsewhere this returns nothing and the prefix heuristics below take
/// over.
fn read_mount_table() -> Vec<MountEntry> {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            // id parent maj:min root mountpoint opts [tags] - fstype source sopts
            let (before, after) = line.split_once(" - ")?;
            let mount_point = before.split_whitespace().nth(4)?;
            let mut tail = after.split_whitespace();
            let fs_type = tail.next()?.to_string();
            let source = tail.next().unwrap_or_default().to_string();
            Some(MountEntry {
                mount_point: PathBuf::from(unescape_mount_field(mount_point)),
                fs_type,
                source: unescape_mount_field(&source),
            })
        })
        .collect()
}

/// mountinfo escapes space, tab, newline and backslash as three octal digits.
fn unescape_mount_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let digits: String = (0..3)
            .filter_map(|_| chars.next_if(|c| ('0'..='7').contains(c)))
            .collect();
        match u32::from_str_radix(&digits, 8).ok().filter(|_| digits.len() == 3) {
            Some(code) => out.push(char::from_u32(code).unwrap_or('\\')),
            None => {
                out.push('\\');
                out.push_str(&digits);
            }
        }
    }
    out
}

fn longest_mount_prefix(mounts: &[MountEntry], path: &Path) -> Option<MountEntry> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.as_os_str().len())
        .cloned()
}

fn kind_for_mount(mount_point: &Path, fs_type: &str, source: &str) -> VolumeKind {
    const NETWORK_FS: &[&str] = &[
        "nfs", "nfs4", "cifs", "smbfs", "smb3", "afs", "fuse.sshfs", "fuse.rclone", "afpfs",
        "webdav", "fuse.davfs",
    ];
    // virtiofs/9p are how Docker Desktop, Lima, and Colima surface host
    // directories into the VM; grpcfuse is Docker Desktop's osxfs successor.
    // A path on one of those is host storage seen across a container boundary.
    const CONTAINER_HOST_FS: &[&str] = &["virtiofs", "9p", "fuse.grpcfuse", "grpcfuse", "osxfs"];

    let lowered = fs_type.to_ascii_lowercase();
    if CONTAINER_HOST_FS.contains(&lowered.as_str()) {
        return VolumeKind::ContainerMount;
    }
    if NETWORK_FS.contains(&lowered.as_str()) {
        return VolumeKind::Network;
    }
    if lowered == "overlay" || lowered == "tmpfs" || lowered == "ramfs" {
        // An overlay at `/` is the container's own root filesystem, fixed for
        // the life of the container. A tmpfs anywhere, and an overlay mounted
        // below the root, are scratch space.
        return if lowered == "overlay" && mount_point == Path::new("/") {
            if in_container() {
                VolumeKind::Fixed
            } else {
                VolumeKind::Unknown
            }
        } else {
            VolumeKind::ContainerMount
        };
    }
    // A bind mount from the host has an ordinary filesystem type but a mount
    // point the runtime created, so inside a container any non-root mount that
    // is not system plumbing is treated as one.
    if in_container()
        && mount_point != Path::new("/")
        && !is_system_mount_point(mount_point)
        && !source.starts_with("udev")
    {
        return VolumeKind::ContainerMount;
    }
    match kind_for_prefix(mount_point) {
        VolumeKind::Unknown => VolumeKind::Fixed,
        other => other,
    }
}

fn is_system_mount_point(mount_point: &Path) -> bool {
    const SYSTEM_PREFIXES: &[&str] = &[
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/etc/hosts",
        "/etc/hostname",
        "/etc/resolv.conf",
    ];
    SYSTEM_PREFIXES
        .iter()
        .any(|prefix| mount_point.starts_with(prefix))
}

/// Classify from well-known layout alone. Deliberately biased toward
/// `Removable`: mistaking a fixed disk for a removable one costs one copy,
/// while the opposite mistake costs a broken project.
fn kind_for_prefix(path: &Path) -> VolumeKind {
    const REMOVABLE_PREFIXES: &[&str] = &["/Volumes", "/media", "/mnt", "/run/media", "/vol"];
    for prefix in REMOVABLE_PREFIXES {
        if path.starts_with(prefix) && path != Path::new(prefix) {
            return VolumeKind::Removable;
        }
    }
    if path.starts_with("/System/Volumes/Data")
        || path.starts_with("/home")
        || path.starts_with("/Users")
        || path.starts_with("/root")
    {
        return VolumeKind::Fixed;
    }
    VolumeKind::Unknown
}

/// The volume root for a path under a conventional mount directory:
/// `/Volumes/<label>`, `/media/<label>`, `/mnt/<label>`,
/// `/run/media/<user>/<label>`.
///
/// This is how a machine with no readable mount table (macOS, and anything
/// without procfs) still knows which directory answers "is the disk plugged
/// in?". Without it every ejected disk would look like a deleted checkout.
fn prefix_mount_point(path: &Path) -> Option<PathBuf> {
    const ROOTS: &[(&str, usize)] = &[
        ("/Volumes", 1),
        ("/media", 1),
        ("/mnt", 1),
        ("/run/media", 2),
        ("/vol", 1),
    ];
    for (root, depth) in ROOTS {
        let root = Path::new(root);
        let Ok(rest) = path.strip_prefix(root) else {
            continue;
        };
        let mut mount = root.to_path_buf();
        let mut taken = 0;
        for component in rest.components() {
            if taken == *depth {
                break;
            }
            if let Component::Normal(part) = component {
                mount.push(part);
                taken += 1;
            }
        }
        if taken == *depth {
            return Some(mount);
        }
    }
    None
}

/// Record where `path` lives, for later availability and link decisions.
pub fn classify_volume(path: &Path) -> VolumeInfo {
    let mounts = read_mount_table();
    if let Some(mount) = longest_mount_prefix(&mounts, path) {
        return VolumeInfo {
            kind: kind_for_mount(&mount.mount_point, &mount.fs_type, &mount.source),
            mount_point: Some(mount.mount_point.display().to_string()),
            fs_type: Some(mount.fs_type),
        };
    }
    VolumeInfo {
        kind: kind_for_prefix(path),
        mount_point: prefix_mount_point(path).map(|mount| mount.display().to_string()),
        fs_type: None,
    }
}

/// Is the volume an entry was registered on currently mounted?
///
/// This asks about the *volume*, never about the entry path. The distinction is
/// the point: a checkout that was deleted is a stale registration to clean up,
/// while a checkout on a disk that is merely unplugged is still real and will
/// be back. Blurring the two would make `zed local prune` quietly forget every
/// project on an external drive that happened not to be attached that
/// afternoon.
pub fn volume_is_present(volume: &VolumeInfo) -> bool {
    let Some(mount_point) = volume.mount_point.as_deref().map(Path::new) else {
        // Nothing recorded to check; the entry path itself decides.
        return true;
    };
    if !mount_point.exists() {
        return false;
    }
    // Linux leaves the mount directory behind after an unmount, so an existing
    // directory is not proof. A directory sharing a device with its parent is
    // no longer a mount point.
    if volume.kind.is_ephemeral() && !is_mount_point(mount_point) {
        return false;
    }
    true
}

/// Is `path` the root of a mounted filesystem? An unknown answer is reported
/// as `true`, so an unreadable device id can never manufacture a false
/// "unavailable".
fn is_mount_point(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return true;
    };
    match (device_id_of(path), device_id_of(parent)) {
        (Some(here), Some(above)) => here != above,
        _ => true,
    }
}

#[cfg(unix)]
fn device_id_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    fs::metadata(path).ok().map(|meta| meta.dev())
}

#[cfg(not(unix))]
fn device_id_of(_path: &Path) -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// path mapping

/// An ordered set of `from -> to` absolute path rewrites.
///
/// The canonical use is a bind mount: `-v /Users/me/codes:/work` makes
/// `ZED_PKG_LOCAL_REGISTRY_PATH_MAP=/Users/me/codes=/work` translate every
/// host-registered entry into the container's view. The same map read backwards
/// lets a `zed local register` performed *inside* the container write a
/// host-shaped path, so one shared index stays meaningful on both sides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathMap {
    rules: Vec<(PathBuf, PathBuf)>,
}

impl PathMap {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The configured rules, host side first, for reporting.
    pub fn rules(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.rules
            .iter()
            .map(|(from, to)| (from.display().to_string(), to.display().to_string()))
    }

    /// Parse `from=to` rules separated by `,` or `;`.
    ///
    /// Both sides must be absolute; a relative side is a configuration error
    /// rather than something to ignore, because it would make lookups depend on
    /// the current directory.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut rules = Vec::new();
        for chunk in raw.split([',', ';']) {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            let (from, to) = chunk
                .split_once('=')
                .with_context(|| format!("invalid path map rule `{chunk}` (expected `from=to`)"))?;
            let from = PathBuf::from(from.trim());
            let to = PathBuf::from(to.trim());
            ensure!(
                from.is_absolute() && to.is_absolute(),
                "path map rule `{chunk}` must use absolute paths on both sides"
            );
            // Canonicalize whichever side exists on *this* machine. A mount is
            // routinely described with a path that is itself reached through a
            // symlink (`/tmp` on macOS is `/private/tmp`), and a rule stated in
            // those terms must still match the canonical paths the index holds.
            rules.push((canonical_or_lexical(&from), canonical_or_lexical(&to)));
        }
        // Longest source prefix wins, so a nested rule refines a broader one
        // regardless of the order they were written in.
        rules.sort_by_key(|(from, _)| std::cmp::Reverse(from.as_os_str().len()));
        Ok(Self { rules })
    }

    pub fn from_env() -> Result<Self> {
        match std::env::var(PATH_MAP_ENV) {
            Ok(raw) if !raw.trim().is_empty() => Self::parse(&raw),
            _ => Ok(Self::default()),
        }
    }

    /// Rewrite a stored (host-side) path into this process's view.
    pub fn to_local(&self, path: &Path) -> PathBuf {
        self.apply(path, false)
    }

    /// Rewrite a path from this process's view back into the stored form, so a
    /// container-side registration lands in an index the host can also read.
    pub fn to_stored(&self, path: &Path) -> PathBuf {
        self.apply(path, true)
    }

    fn apply(&self, path: &Path, reverse: bool) -> PathBuf {
        for (from, to) in &self.rules {
            let (from, to) = if reverse { (to, from) } else { (from, to) };
            if let Ok(rest) = path.strip_prefix(from) {
                return if rest.as_os_str().is_empty() {
                    to.clone()
                } else {
                    to.join(rest)
                };
            }
        }
        path.to_path_buf()
    }
}

/// Canonicalize a path if it exists here, otherwise normalize it lexically.
/// The far side of a host/container mapping legitimately does not exist on this
/// machine, so a failure to canonicalize is expected, not an error.
fn canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_lexically(path))
}

/// Resolve `.` and `..` without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

// ---------------------------------------------------------------------------
// link policy

/// How a registered checkout should reach `zed_modules/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkPolicy {
    /// Symlink from stable media, copy from ephemeral media.
    #[default]
    Auto,
    /// Always symlink; the caller accepts a dangling link if the media leaves.
    Symlink,
    /// Always copy. This is what a container image build needs: the layer must
    /// not depend on a mount that only existed during the build.
    Copy,
}

impl LinkPolicy {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "symlink" | "link" => Ok(Self::Symlink),
            "copy" => Ok(Self::Copy),
            other => bail!("invalid local link policy `{other}` (expected auto, symlink, or copy)"),
        }
    }

    pub fn from_env() -> Result<Self> {
        match std::env::var(LINK_POLICY_ENV) {
            Ok(raw) => Self::parse(&raw),
            Err(_) => Ok(Self::Auto),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Symlink => "symlink",
            Self::Copy => "copy",
        }
    }

    /// Decide whether a checkout may be symlinked.
    ///
    /// `project_on_same_volume` describes the *consumer*: when the project tree
    /// itself lives on the same volume as the checkout, a link between them
    /// survives exactly as long as anything else on that volume, so there is
    /// nothing to protect against and linking stays allowed.
    pub fn resolve(
        self,
        source: VolumeKind,
        project_on_same_volume: bool,
        ephemeral_override: bool,
    ) -> LinkDecision {
        match self {
            Self::Symlink => LinkDecision::Symlink,
            Self::Copy => LinkDecision::Copy,
            Self::Auto => {
                if ephemeral_override {
                    LinkDecision::Copy
                } else if source.is_ephemeral() && !project_on_same_volume {
                    LinkDecision::Copy
                } else {
                    LinkDecision::Symlink
                }
            }
        }
    }
}

/// The resolved materialization choice for one registered checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkDecision {
    Symlink,
    Copy,
}

/// This machine's view of the filesystem, as far as the local registry is
/// concerned. Grouped into one struct so [`Config`] gains one field rather than
/// four, and so a caller with no opinion can pass [`Default::default`].
#[derive(Debug, Clone, Default)]
pub struct LocalPortability {
    /// Unparsed `from=to` rules; `None` falls back to [`PATH_MAP_ENV`].
    pub path_map: Option<String>,
    /// `None` falls back to [`LINK_POLICY_ENV`].
    pub link_policy: Option<LinkPolicy>,
    /// Copy every registered checkout regardless of the volume it is on.
    pub ephemeral: bool,
}

impl LocalPortability {
    pub fn resolved_path_map(&self) -> Result<PathMap> {
        match &self.path_map {
            Some(raw) if !raw.trim().is_empty() => PathMap::parse(raw),
            Some(_) => Ok(PathMap::default()),
            None => PathMap::from_env(),
        }
    }

    pub fn resolved_link_policy(&self) -> Result<LinkPolicy> {
        match self.link_policy {
            Some(policy) => Ok(policy),
            None => LinkPolicy::from_env(),
        }
    }

    pub fn resolved_ephemeral(&self) -> bool {
        self.ephemeral
            || std::env::var(EPHEMERAL_ENV)
                .ok()
                .is_some_and(|value| truthy(&value))
    }
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// How one selected checkout should be materialized for `project`.
pub fn link_decision(cfg: &Config, entry: &LocalEntry, project: &Path) -> Result<LinkDecision> {
    let process_policy = cfg.local.resolved_link_policy()?;
    // An explicit process-wide policy is an operator decision for this run and
    // wins over the preference recorded when the checkout was registered.
    let policy = match process_policy {
        LinkPolicy::Auto => entry.link_policy,
        explicit => explicit,
    };
    let same_volume = device_id_of(project)
        .zip(device_id_of(&entry.path_buf()))
        .map(|(here, there)| here == there)
        .unwrap_or(false);
    Ok(policy.resolve(
        entry.volume.kind,
        same_volume,
        cfg.local.resolved_ephemeral(),
    ))
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
    // Translate host paths into this process's view *once*, here, so every
    // consumer downstream — health, selection, materialization — works in the
    // only coordinate system it can actually open.
    apply_path_map(&mut index, &cfg.local.resolved_path_map()?);
    Ok(index)
}

/// Rewrite entry paths into the local view, remembering the stored form.
fn apply_path_map(index: &mut LocalIndex, map: &PathMap) {
    if map.is_empty() {
        return;
    }
    for entry in &mut index.entries {
        let stored = entry.path.clone();
        let local = map.to_local(Path::new(&stored));
        let Some(local) = local.to_str() else {
            continue;
        };
        if local != stored {
            entry.stored_path = Some(stored);
            entry.path = local.to_string();
        }
    }
}

/// The inverse: put stored (host) paths back before the index is written, so a
/// registration made inside a container stays readable on the host.
fn unapply_path_map(index: &LocalIndex, map: &PathMap) -> LocalIndex {
    let mut out = index.clone();
    for entry in &mut out.entries {
        if let Some(stored) = entry.stored_path.take() {
            entry.path = stored;
            continue;
        }
        if map.is_empty() {
            continue;
        }
        if let Some(stored) = map.to_stored(Path::new(&entry.path)).to_str() {
            entry.path = stored.to_string();
        }
    }
    out
}

/// Write the index atomically: staged beside the final path, then renamed.
fn save(cfg: &Config, index: &LocalIndex) -> Result<()> {
    let path = index_path(cfg)?;
    let parent = index_parent(cfg)?;
    secure_create_dir(&parent)?;
    let mut stored = unapply_path_map(index, &cfg.local.resolved_path_map()?);
    // Sort in the *stored* coordinate system so the file has one canonical
    // order no matter which side of a bind mount wrote it last.
    stored.normalize();
    let text = format!("{}\n", serde_json::to_string_pretty(&stored)?);
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
    /// The volume this checkout lives on is not mounted right now. Distinct
    /// from `MissingPath`: an unplugged disk is not a broken registration, and
    /// `zed local prune` must not forget it.
    VolumeUnavailable { mount_point: Option<String> },
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
            Self::VolumeUnavailable { mount_point } => match mount_point {
                Some(mount) => format!("unavailable: {mount} is not mounted"),
                None => "unavailable: the volume is not reachable".to_string(),
            },
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
    // Ask about the volume before asking about the directory. If the disk is
    // not mounted, "the directory is gone" would be a true statement and a
    // misleading diagnosis.
    if !volume_is_present(&entry.volume) {
        return (
            EntryHealth::VolumeUnavailable {
                mount_point: entry.volume.mount_point.clone(),
            },
            None,
        );
    }
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
    link_policy: Option<LinkPolicy>,
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
            volume: classify_volume(&dir),
            link_policy: link_policy.unwrap_or_else(|| {
                existing
                    .map(|position| index.entries[position].link_policy)
                    .unwrap_or_default()
            }),
            stored_path: None,
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
            // Shelved is not broken, and unplugged is not deleted: neither is
            // evidence that the registration was a mistake.
            EntryHealth::Ok { .. }
            | EntryHealth::Disabled
            | EntryHealth::VolumeUnavailable { .. } => None,
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
        let (action, _) = register(cfg, &hit.dir, priority, true, None)?;
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
            local: Default::default(),
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

    // -- portability: volumes, path maps, link policy ----------------------

    #[test]
    fn well_known_prefixes_classify_removable_media() {
        assert_eq!(
            kind_for_prefix(Path::new("/Volumes/Backup/x")),
            VolumeKind::Removable
        );
        assert_eq!(
            kind_for_prefix(Path::new("/media/usb/x")),
            VolumeKind::Removable
        );
        assert_eq!(
            kind_for_prefix(Path::new("/mnt/data/x")),
            VolumeKind::Removable
        );
        assert_eq!(
            kind_for_prefix(Path::new("/Users/me/codes")),
            VolumeKind::Fixed
        );
        assert_eq!(kind_for_prefix(Path::new("/opt/x")), VolumeKind::Unknown);
        assert_eq!(
            kind_for_prefix(Path::new("/Volumes")),
            VolumeKind::Unknown,
            "the container of mount points is not itself removable media"
        );
    }

    #[test]
    fn docker_desktop_host_filesystems_are_container_mounts() {
        for fs_type in ["virtiofs", "fuse.grpcfuse", "9p"] {
            assert_eq!(
                kind_for_mount(Path::new("/work"), fs_type, "host"),
                VolumeKind::ContainerMount,
                "{fs_type}"
            );
        }
        assert_eq!(
            kind_for_mount(Path::new("/net"), "nfs4", "server:/export"),
            VolumeKind::Network
        );
    }

    #[test]
    fn only_ephemeral_volume_kinds_force_a_copy() {
        for kind in [VolumeKind::Fixed, VolumeKind::Unknown] {
            assert!(!kind.is_ephemeral(), "{kind:?}");
        }
        for kind in [
            VolumeKind::Removable,
            VolumeKind::Network,
            VolumeKind::ContainerMount,
        ] {
            assert!(kind.is_ephemeral(), "{kind:?}");
        }
    }

    #[test]
    fn mountinfo_fields_are_unescaped() {
        assert_eq!(unescape_mount_field(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount_field("/mnt/plain"), "/mnt/plain");
        assert_eq!(unescape_mount_field(r"/mnt/back\slash"), r"/mnt/back\slash");
    }

    #[test]
    fn the_longest_mount_prefix_wins() {
        let mounts = vec![
            MountEntry {
                mount_point: PathBuf::from("/"),
                fs_type: "ext4".into(),
                source: "/dev/sda1".into(),
            },
            MountEntry {
                mount_point: PathBuf::from("/mnt/data"),
                fs_type: "exfat".into(),
                source: "/dev/sdb1".into(),
            },
        ];
        let chosen = longest_mount_prefix(&mounts, Path::new("/mnt/data/codes/kit")).unwrap();
        assert_eq!(chosen.mount_point, PathBuf::from("/mnt/data"));
    }

    #[test]
    fn a_conventional_mount_root_is_recovered_without_a_mount_table() {
        assert_eq!(
            prefix_mount_point(Path::new("/Volumes/Backup/codes/kit")),
            Some(PathBuf::from("/Volumes/Backup"))
        );
        assert_eq!(
            prefix_mount_point(Path::new("/run/media/alex/usb/kit")),
            Some(PathBuf::from("/run/media/alex/usb"))
        );
        assert_eq!(
            prefix_mount_point(Path::new("/Volumes")),
            None,
            "the container of mount points names no single volume"
        );
        assert_eq!(prefix_mount_point(Path::new("/Users/me/codes")), None);
    }

    #[test]
    fn an_ejected_disk_is_unavailable_while_a_present_volume_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("mount");
        fs::create_dir_all(&mount).unwrap();

        // The disk is gone: its mount point does not exist at all.
        assert!(!volume_is_present(&VolumeInfo {
            kind: VolumeKind::Removable,
            mount_point: Some(temp.path().join("absent").display().to_string()),
            fs_type: None,
        }));
        // The mount point exists but nothing is mounted on it — what Linux
        // leaves behind after an unmount.
        assert!(!volume_is_present(&VolumeInfo {
            kind: VolumeKind::Removable,
            mount_point: Some(mount.display().to_string()),
            fs_type: None,
        }));
        // A fixed volume is never second-guessed this way: an ordinary
        // directory on the system disk is not a mount point either.
        assert!(volume_is_present(&VolumeInfo {
            kind: VolumeKind::Fixed,
            mount_point: Some(mount.display().to_string()),
            fs_type: None,
        }));
        assert!(volume_is_present(&VolumeInfo::default()));
    }

    #[test]
    fn an_unmounted_volume_reports_unavailable_rather_than_a_missing_directory() {
        let entry = LocalEntry {
            org: "acme".into(),
            name: "widget".into(),
            version: "1.0.0".into(),
            path: "/Volumes/Scratch/widget".into(),
            priority: 0,
            enabled: true,
            manifest_sha256: None,
            registered_at: None,
            volume: VolumeInfo {
                kind: VolumeKind::Removable,
                mount_point: Some("/Volumes/Scratch-does-not-exist".into()),
                fs_type: None,
            },
            link_policy: LinkPolicy::Auto,
            stored_path: None,
        };
        let (state, manifest) = health(&entry);
        assert!(
            matches!(state, EntryHealth::VolumeUnavailable { .. }),
            "{state:?}"
        );
        assert!(manifest.is_none());
        assert!(state.label().contains("not mounted"), "{}", state.label());
    }

    #[test]
    fn path_map_rewrites_both_directions_with_longest_prefix_wins() {
        let map = PathMap::parse("/host/codes=/work,/host/codes/vendor=/vendor").unwrap();
        assert_eq!(
            map.to_local(Path::new("/host/codes/kit")),
            PathBuf::from("/work/kit")
        );
        assert_eq!(
            map.to_local(Path::new("/host/codes/vendor/kit")),
            PathBuf::from("/vendor/kit"),
            "the more specific rule wins regardless of declaration order"
        );
        assert_eq!(
            map.to_stored(Path::new("/work/kit")),
            PathBuf::from("/host/codes/kit")
        );
        assert_eq!(
            map.to_local(Path::new("/elsewhere/kit")),
            PathBuf::from("/elsewhere/kit"),
            "unmapped paths pass through untouched"
        );
        assert_eq!(
            map.to_local(Path::new("/host/codes")),
            PathBuf::from("/work"),
            "the prefix itself maps, not only its children"
        );
    }

    #[test]
    fn path_map_rules_canonicalize_the_side_that_exists_here() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let linked = temp.path().join("linked");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        let map = PathMap::parse(&format!("/host/src={}", linked.display())).unwrap();
        let canonical = fs::canonicalize(&real).unwrap();
        assert_eq!(
            map.to_local(Path::new("/host/src/kit")),
            canonical.join("kit"),
            "a rule written through a symlink still lands on the canonical path"
        );
        assert_eq!(
            map.to_stored(&canonical.join("kit")),
            PathBuf::from("/host/src/kit")
        );
    }

    #[test]
    fn path_map_rejects_relative_rules() {
        assert!(PathMap::parse("codes=/work").is_err());
        assert!(PathMap::parse("/codes=work").is_err());
        assert!(PathMap::parse("nonsense").is_err());
        assert!(PathMap::parse("").unwrap().is_empty());
    }

    #[test]
    fn a_host_shaped_index_is_read_and_written_through_the_path_map() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let container_root = root.join("work");
        let project = write_project(&container_root, "acme", "widget", "1.0.0");
        let mut cfg = config_for(&root.join("home"));
        cfg.local.path_map = Some(format!("/host/codes={}", container_root.display()));

        let (_, entry) = register(&cfg, &project, None, true, None).unwrap();
        assert_eq!(
            entry.path,
            project.display().to_string(),
            "the caller sees the path it can actually open"
        );

        // What landed on disk is the host spelling, so the host can read it.
        let raw = fs::read_to_string(index_path(&cfg).unwrap()).unwrap();
        assert!(raw.contains("/host/codes/"), "{raw}");
        assert!(!raw.contains(&container_root.display().to_string()), "{raw}");

        // Reading it back maps it into this process's view again.
        let index = load(&cfg).unwrap();
        assert_eq!(index.entries[0].path, project.display().to_string());
        assert!(
            index.entries[0]
                .stored_path
                .as_deref()
                .is_some_and(|stored| stored.starts_with("/host/codes/"))
        );
        assert!(health(&index.entries[0]).0.is_selectable());
    }

    #[test]
    fn a_host_shaped_index_without_a_map_simply_does_not_resolve_here() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let container_root = root.join("work");
        let project = write_project(&container_root, "acme", "widget", "1.0.0");

        let mut mapped = config_for(&root.join("home"));
        mapped.local.path_map = Some(format!("/host/codes={}", container_root.display()));
        register(&mapped, &project, None, true, None).unwrap();

        // Same index, same home, no mapping configured.
        let unmapped = config_for(&root.join("home"));
        let index = load(&unmapped).unwrap();
        assert!(index.entries[0].path.starts_with("/host/codes/"));
        assert!(
            !health(&index.entries[0]).0.is_selectable(),
            "an unmapped host path must not silently resolve to something else"
        );
    }

    #[test]
    fn auto_policy_symlinks_from_fixed_media_and_copies_from_ephemeral_media() {
        for kind in [VolumeKind::Fixed, VolumeKind::Unknown] {
            assert_eq!(
                LinkPolicy::Auto.resolve(kind, false, false),
                LinkDecision::Symlink,
                "{kind:?}"
            );
        }
        for kind in [
            VolumeKind::Removable,
            VolumeKind::Network,
            VolumeKind::ContainerMount,
        ] {
            assert_eq!(
                LinkPolicy::Auto.resolve(kind, false, false),
                LinkDecision::Copy,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_project_on_the_same_removable_disk_may_still_be_symlinked() {
        assert_eq!(
            LinkPolicy::Auto.resolve(VolumeKind::Removable, true, false),
            LinkDecision::Symlink,
            "nothing outlives the disk either way, so the link costs nothing"
        );
    }

    #[test]
    fn the_ephemeral_override_forces_copies_for_container_builds() {
        assert_eq!(
            LinkPolicy::Auto.resolve(VolumeKind::Fixed, true, true),
            LinkDecision::Copy,
            "a build layer must not point at a mount that ends with the step"
        );
        assert_eq!(
            LinkPolicy::Symlink.resolve(VolumeKind::Removable, false, true),
            LinkDecision::Symlink,
            "an explicit symlink policy stays an explicit operator decision"
        );
    }

    #[test]
    fn link_policy_parsing_round_trips_and_rejects_nonsense() {
        for (raw, expected) in [
            ("auto", LinkPolicy::Auto),
            ("", LinkPolicy::Auto),
            ("symlink", LinkPolicy::Symlink),
            ("link", LinkPolicy::Symlink),
            ("COPY", LinkPolicy::Copy),
        ] {
            assert_eq!(LinkPolicy::parse(raw).unwrap(), expected, "{raw}");
        }
        assert!(LinkPolicy::parse("hardlink").is_err());
    }

    #[test]
    fn a_process_wide_link_policy_overrides_the_per_entry_preference() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = write_project(&root, "acme", "widget", "1.0.0");
        let mut cfg = config_for(&root.join("home"));

        let (_, entry) = register(&cfg, &project, None, true, Some(LinkPolicy::Symlink)).unwrap();
        assert_eq!(
            link_decision(&cfg, &entry, &root).unwrap(),
            LinkDecision::Symlink
        );

        cfg.local.link_policy = Some(LinkPolicy::Copy);
        assert_eq!(
            link_decision(&cfg, &entry, &root).unwrap(),
            LinkDecision::Copy
        );
    }

    #[test]
    fn a_registration_records_the_volume_it_lives_on() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = write_project(&root, "acme", "widget", "1.0.0");
        let cfg = config_for(&root.join("home"));
        let (_, entry) = register(&cfg, &project, None, true, None).unwrap();
        // The kind depends on where the test runner's temp directory lives, so
        // assert the invariant that matters: something was recorded, and it
        // round-trips through the index unchanged.
        let reloaded = load(&cfg).unwrap();
        assert_eq!(reloaded.entries[0].volume, entry.volume);
        assert_eq!(reloaded.entries[0].link_policy, LinkPolicy::Auto);
    }

    #[test]
    fn prune_keeps_an_unplugged_volume_but_drops_a_deleted_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let present = write_project(&root, "acme", "here", "1.0.0");
        let gone = write_project(&root, "acme", "gone", "1.0.0");
        let cfg = config_for(&root.join("home"));
        register(&cfg, &present, None, true, None).unwrap();
        register(&cfg, &gone, None, true, None).unwrap();

        // Rewrite one entry to look like it lives on a disk that is no longer
        // attached, and delete the other outright.
        let mut index = load(&cfg).unwrap();
        for entry in &mut index.entries {
            if entry.name == "here" {
                entry.volume = VolumeInfo {
                    kind: VolumeKind::Removable,
                    mount_point: Some(root.join("not-mounted").display().to_string()),
                    fs_type: None,
                };
            }
        }
        save(&cfg, &index).unwrap();
        fs::remove_dir_all(&gone).unwrap();

        let dropped = prune(&cfg, false).unwrap();
        let dropped_names: Vec<&str> = dropped
            .iter()
            .map(|status| status.entry.name.as_str())
            .collect();
        assert_eq!(dropped_names, ["gone"]);
        let remaining = load(&cfg).unwrap();
        assert_eq!(remaining.entries.len(), 1);
        assert_eq!(remaining.entries[0].name, "here");
    }

}
