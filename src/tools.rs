//! Declared command-line tools — the `[tool-dependencies]` table.
//!
//! npm's `devDependencies` conflate two unrelated needs: *code this project
//! links against* and *a program a developer runs while working on it*. Only
//! the first has to live inside the project. Treating the second the same way
//! is what puts a hundred copies of one linter on a disk that needed two.
//!
//! Zed separates them. `[dependencies]` are resolved, materialized into the
//! project's install dir, and wired into the native toolchain.
//! `[tool-dependencies]` are resolved and pinned in `.zpkg.lock` — and that is
//! all `zed install` does with them. Nothing is downloaded and nothing is
//! written into the project tree. The declaration exists so the required
//! version is reviewable, diffable, and identical for everyone on the team;
//! the bytes live once per version in the central profile store that
//! [`crate::global`] already manages:
//!
//! ```text
//! <ZED_PKG_HOME>/global/profiles/<org>/<name>/<version>/zed_modules/.bin/<cmd>
//! ```
//!
//! Two projects pinning the same version share one copy. Two projects pinning
//! different versions get two copies in the store and none in either project.
//! `zed tools sync` provisions what a project's pins need; `zed run <cmd>`
//! executes the pinned version. See zed-docs 36.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{BIN_DIR, LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};
use zed_interfaces::registry::VersionMetadata;
use zed_interfaces::version::{self, Requirement};

use crate::cli::{InstallMode, ToolsPolicy};
use crate::config::Config;
use crate::global;
use crate::manifestless;
use crate::registry::Registry;
use crate::store::Store;

/// One declared tool, joined with whatever the lockfile and the central store
/// currently say about it.
#[derive(Debug, Clone)]
pub struct ToolPin {
    /// `org/name`.
    pub key: String,
    /// The requirement as authored in `[tool-dependencies]`.
    pub requirement: String,
    /// The exact pin from `.zpkg.lock`, when the project has been resolved.
    pub locked: Option<LockedPackage>,
    /// The central profile directory for the pinned version, when it is
    /// already provisioned on this machine.
    pub profile: Option<PathBuf>,
}

impl ToolPin {
    pub fn version(&self) -> Option<&str> {
        self.locked.as_ref().map(|locked| locked.version.as_str())
    }

    /// `org/name@version` once resolved, else the bare `org/name`.
    pub fn label(&self) -> String {
        match self.version() {
            Some(version) => format!("{}@{version}", self.key),
            None => self.key.clone(),
        }
    }

    /// Human-readable state, in the vocabulary the CLI prints.
    pub fn status(&self) -> &'static str {
        match (&self.locked, &self.profile) {
            (None, _) => "unresolved",
            (Some(_), None) => "not provisioned",
            (Some(_), Some(_)) => "ready",
        }
    }
}

fn split_key(key: &str) -> Result<(&str, &str)> {
    key.split_once('/')
        .filter(|(org, name)| !org.is_empty() && !name.is_empty())
        .with_context(|| format!("invalid tool dependency key `{key}` (expected org/name)"))
}

fn read_project_manifest(project: &Path) -> Result<Option<Manifest>> {
    let path = project.join(MANIFEST_FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    Manifest::parse(&text)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

fn read_project_lock(project: &Path) -> Option<Lockfile> {
    let text = fs::read_to_string(project.join(LOCKFILE_FILE)).ok()?;
    Lockfile::parse(&text).ok()
}

/// The central profile directory for one exact pinned version, if it exists.
fn provisioned_profile(cfg: &Config, key: &str, version: &str) -> Option<PathBuf> {
    let package_root = global::profile_root(cfg, key).ok()?;
    let root = global::version_dir(&package_root, version).ok()?;
    global::is_profile_dir(&root).then_some(root)
}

/// Every tool this project declares, joined with lock and store state.
pub fn declared(cfg: &Config, project: &Path) -> Result<Vec<ToolPin>> {
    let Some(manifest) = read_project_manifest(project)? else {
        return Ok(Vec::new());
    };
    let lock = read_project_lock(project);
    let mut pins = Vec::new();
    for (key, requirement) in &manifest.tool_dependencies {
        let (org, name) = split_key(key)?;
        let locked = lock
            .as_ref()
            .and_then(|lock| lock.find_tool(org, name))
            .cloned();
        let profile = locked
            .as_ref()
            .and_then(|locked| provisioned_profile(cfg, key, &locked.version));
        pins.push(ToolPin {
            key: key.clone(),
            requirement: requirement.clone(),
            locked,
            profile,
        });
    }
    Ok(pins)
}

/// Resolve `[tool-dependencies]` to exact versions.
///
/// Resolution only: this asks the registry which version satisfies each
/// requirement and never fetches an artifact. That is the whole point of a
/// tool declaration — the project records *which* version it needs without
/// making every checkout pay to materialize it.
pub fn resolve_pins(
    manifest: &Manifest,
    reg: &dyn Registry,
) -> Result<BTreeMap<String, VersionMetadata>> {
    let mut resolved = BTreeMap::new();
    for (key, requirement) in &manifest.tool_dependencies {
        let (org, name) = split_key(key)?;
        let req = Requirement::parse(requirement);
        let package = reg.get_package(org, name)?;
        let mut candidates = package.versions.clone();
        let mut skipped_yanked: Vec<String> = Vec::new();
        let metadata = loop {
            let Some(version) = version::resolve(&req, &candidates).map(str::to_string) else {
                if !skipped_yanked.is_empty() {
                    bail!(
                        "the only version(s) of tool {key} satisfying `{requirement}` are yanked ({})",
                        skipped_yanked.join(", ")
                    );
                }
                bail!(
                    "no version of tool {key} satisfies `{requirement}` (available: {})",
                    package.versions.join(", ")
                );
            };
            let metadata = reg.get_version(org, name, &version)?;
            if metadata.yanked {
                candidates.retain(|candidate| *candidate != version);
                skipped_yanked.push(version);
                continue;
            }
            break metadata;
        };
        resolved.insert(key.clone(), metadata);
    }
    Ok(resolved)
}

/// A frozen install must not silently accept a tool pin that no longer
/// satisfies the manifest, for exactly the reason a package pin must not.
pub fn validate_frozen_pins(manifest: &Manifest, lock: &Lockfile) -> Result<()> {
    for (key, requirement) in &manifest.tool_dependencies {
        let (org, name) = split_key(key)?;
        let entry = lock
            .find_tool(org, name)
            .with_context(|| format!("--frozen: tool `{key}` is not in {LOCKFILE_FILE}"))?;
        if !Requirement::parse(requirement).matches(&entry.version) {
            bail!(
                "--frozen: lockfile pins tool {key}@{} which no longer satisfies `{requirement}`",
                entry.version
            );
        }
    }
    Ok(())
}

/// One line for the end of `zed install`, or nothing when the project declares
/// no tools.
///
/// This reports the *pins*, which is all the resolver itself did: no tool byte
/// reached the project tree. Whether the pinned versions are then fetched into
/// the central store is a separate, policy-controlled step the CLI performs
/// afterwards ([`after_install`]), so this line never tells the reader to go
/// run something that is about to happen two lines further down.
pub fn install_summary(cfg: &Config, project: &Path) -> Option<String> {
    let pins = declared(cfg, project).ok()?;
    if pins.is_empty() {
        return None;
    }
    let missing: Vec<&ToolPin> = pins.iter().filter(|pin| pin.profile.is_none()).collect();
    if missing.is_empty() {
        return Some(format!(
            "{} declared tool(s) pinned in {LOCKFILE_FILE}, all present in the central tool store",
            pins.len()
        ));
    }
    Some(format!(
        "{} declared tool(s) pinned in {LOCKFILE_FILE}; {} not yet in the central tool store ({})",
        pins.len(),
        missing.len(),
        labels(&missing).join(", ")
    ))
}

/// `org/name@version` for each pin, falling back to the bare identity for a
/// pin the lockfile has not resolved yet.
fn labels(pins: &[&ToolPin]) -> Vec<String> {
    pins.iter().map(|pin| pin.label()).collect()
}

/// Provision every pinned tool this project needs into the central store.
///
/// A version already present is left untouched, which is the common case in a
/// team or a monorepo: the second project that pins it does no work and adds
/// no bytes. Nothing here touches `PATH` — a project declaring a tool is not
/// permission to change what a bare `lint` means in the user's shell.
pub fn sync(
    cfg: &Config,
    project: &Path,
    install_mode: InstallMode,
    allow_build: bool,
) -> Result<usize> {
    let pins = declared(cfg, project)?;
    if pins.is_empty() {
        println!("no [tool-dependencies] declared in {MANIFEST_FILE}");
        return Ok(0);
    }
    let unresolved: Vec<&ToolPin> = pins.iter().filter(|pin| pin.locked.is_none()).collect();
    if !unresolved.is_empty() {
        bail!(
            "tool(s) {} are declared but not pinned in {LOCKFILE_FILE}; run `zed install` first",
            unresolved
                .iter()
                .map(|pin| pin.key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let _lock = global::acquire_lock(cfg)?;
    let mut provisioned = 0usize;
    for pin in &pins {
        if provision_pin(cfg, pin, install_mode, allow_build)? {
            println!("provisioned {}", pin.label());
            provisioned += 1;
        }
    }
    let shims = write_shims(cfg, project)?;
    if !shims.is_empty() {
        println!(
            "{} tool shim(s) in {}/{BIN_DIR}/ ({})",
            shims.len(),
            modules_dir_name(project),
            shims.join(", ")
        );
    }
    Ok(provisioned)
}

/// Write one pinned tool into the central store, or leave it alone because it
/// is already there. Returns whether anything was written.
///
/// Callers hold the global tool-store lock: two projects installing the same
/// pinned tool at the same time must not both stage into it.
fn provision_pin(
    cfg: &Config,
    pin: &ToolPin,
    install_mode: InstallMode,
    allow_build: bool,
) -> Result<bool> {
    let Some(locked) = &pin.locked else {
        bail!(
            "tool `{}` is declared but not pinned in {LOCKFILE_FILE}; run `zed install` first",
            pin.key
        );
    };
    let package_root = global::profile_root(cfg, &pin.key)?;
    let root = global::version_dir(&package_root, &locked.version)?;
    // The common case in a team or a monorepo: the second project pinning this
    // version does no work and adds no bytes.
    if global::is_profile_dir(&root) {
        return Ok(false);
    }
    fs::create_dir_all(&package_root)?;
    let staging = global::staging_dir(&package_root)?;
    fs::create_dir_all(&staging)?;
    // `=` rather than the authored range: the lockfile already decided,
    // and provisioning must not get a second opinion from the registry.
    let spec = format!("{}@={}", pin.key, locked.version);
    // The staging directory is the project. Ancestor discovery would walk out
    // of <ZED_PKG_HOME> and could land on a stray manifest in the user's home.
    let outcome = manifestless::install_exact_root(
        &staging,
        cfg,
        std::slice::from_ref(&spec),
        false,
        install_mode,
        allow_build,
        None,
    );
    if let Err(error) = outcome {
        let _ = global::remove_path_if_present(&staging);
        return Err(error)
            .with_context(|| format!("provisioning tool {}@{}", pin.key, locked.version));
    }
    global::write_profile_metadata(&staging, &pin.key, &spec, &locked.version)?;
    if let Err(error) = fs::rename(&staging, &root) {
        let _ = global::remove_path_if_present(&staging);
        return Err(error).with_context(|| format!("promoting tool profile to {}", root.display()));
    }
    // Follow the store references from the staging path to the promoted one,
    // or `zed gc` will prune the entries this profile's symlinks point into.
    Store::new(&cfg.home).relocate_project(&staging, &root)?;
    Ok(true)
}

/// The executable a declared tool exposes under `command`, if this project
/// pins a tool that provides it and that version is provisioned.
pub fn locate(cfg: &Config, project: &Path, command: &str) -> Option<PathBuf> {
    let pins = declared(cfg, project).ok()?;
    for pin in pins {
        let Some(profile) = pin.profile else { continue };
        let candidate = profile.join(MODULES_DIR).join(BIN_DIR).join(command);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `zed tools list`.
pub fn print_list(cfg: &Config, project: &Path) -> Result<i32> {
    let pins = declared(cfg, project)?;
    if pins.is_empty() {
        println!("no [tool-dependencies] declared in {MANIFEST_FILE}");
        return Ok(0);
    }
    for pin in &pins {
        println!(
            "{} {} -> {} [{}]{}",
            pin.key,
            pin.requirement,
            pin.version().unwrap_or("unresolved"),
            pin.status(),
            match &pin.profile {
                Some(profile) => format!(" {}", profile.display()),
                None => String::new(),
            }
        );
    }
    Ok(0)
}

/// `zed tools which <command>`.
pub fn print_which(cfg: &Config, project: &Path, command: &str) -> Result<i32> {
    match locate(cfg, project, command) {
        Some(path) => {
            println!("{}", path.display());
            Ok(0)
        }
        None => {
            bail!(
                "no provisioned tool in this project exposes `{command}`; \
                 `zed tools list` shows what is declared and whether it is provisioned"
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Provisioning policy, and the shims that make a pinned tool runnable

/// Marker written into every generated shim. It is what makes pruning safe:
/// zed removes a stale shim only when it can prove zed wrote it, so a hoisted
/// package binary or a file a developer dropped in by hand is never deleted.
pub const SHIM_MARKER: &str = "zed-tool-shim/v1";

/// What one provisioning pass did, per tool.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    /// Tools written into the central store by this pass.
    pub provisioned: Vec<String>,
    /// Tools the central store already had at the pinned version.
    pub already_present: Vec<String>,
    /// Tools that could not be provisioned, with the reason.
    pub failures: Vec<(String, String)>,
}

/// Provision every pinned tool this project needs, collecting rather than
/// propagating per-tool failures.
///
/// Collecting is deliberate. This runs after `zed install` has already
/// committed the project's real dependencies, and a registry that went away
/// while fetching a formatter must not retroactively invalidate a correct
/// install. The caller decides what a failure means: [`ToolsPolicy::Auto`]
/// reports it, [`ToolsPolicy::Require`] fails on it.
pub fn provision_all(
    cfg: &Config,
    project: &Path,
    install_mode: InstallMode,
    allow_build: bool,
) -> Result<ProvisionReport> {
    let pins = declared(cfg, project)?;
    let mut report = ProvisionReport::default();
    if pins.is_empty() {
        return Ok(report);
    }
    let _lock = global::acquire_lock(cfg)?;
    for pin in &pins {
        let label = pin.label();
        match provision_pin(cfg, pin, install_mode, allow_build) {
            Ok(true) => report.provisioned.push(label),
            Ok(false) => report.already_present.push(label),
            Err(error) => report.failures.push((label, format!("{error:#}"))),
        }
    }
    Ok(report)
}

/// The CLI's post-install tool step: provision under the chosen policy, then
/// refresh the project's shims.
///
/// This lives in the CLI path rather than inside `ops::install` on purpose.
/// Resolving and pinning a tool is part of solving the project — a library
/// caller gets that unconditionally. Reaching the network to fetch a linter is
/// not, so it stays where a human passed a flag.
pub fn after_install(
    cfg: &Config,
    project: &Path,
    policy: ToolsPolicy,
    install_mode: InstallMode,
    allow_build: bool,
) -> Result<()> {
    let pins = declared(cfg, project)?;
    // Note the missing early return for an empty declaration: a project that
    // just *dropped* its last tool has no pins and still has a stale shim to
    // prune, so the shim pass below always runs.
    if !pins.is_empty() {
        match policy {
            ToolsPolicy::Skip => {
                let missing: Vec<&ToolPin> =
                    pins.iter().filter(|pin| pin.profile.is_none()).collect();
                if !missing.is_empty() {
                    println!(
                        "--tools=skip: {} pinned tool(s) left unprovisioned ({}); `zed tools sync` provisions them",
                        missing.len(),
                        labels(&missing).join(", ")
                    );
                }
            }
            ToolsPolicy::Auto | ToolsPolicy::Require => {
                let report = provision_all(cfg, project, install_mode, allow_build)?;
                for label in &report.provisioned {
                    println!("provisioned {label} in the central tool store");
                }
                for (label, error) in &report.failures {
                    eprintln!("warning: could not provision tool {label}: {error}");
                }
                if policy == ToolsPolicy::Require && !report.failures.is_empty() {
                    bail!(
                        "--tools=require: {} declared tool(s) could not be provisioned",
                        report.failures.len()
                    );
                }
            }
        }
    }
    let shims = write_shims(cfg, project)?;
    if !shims.is_empty() {
        println!(
            "{} tool shim(s) in {}/{BIN_DIR}/ ({})",
            shims.len(),
            modules_dir_name(project),
            shims.join(", ")
        );
    }
    Ok(())
}

/// Make a command runnable before `zed run` looks for it, provisioning pinned
/// tools that have never been fetched on this machine.
///
/// A tool's command names live in its own manifest, which is only readable
/// once the tool is in the store — so "is `eslint` one of my declared tools?"
/// cannot be answered without provisioning first. Rather than guess, this
/// provisions every pin that is missing, but only while any pin is missing: on
/// a fresh clone it runs once, and every later `zed run` finds nothing to do.
pub fn ensure_available(cfg: &Config, project: &Path, command: &str) -> Result<()> {
    if locate(cfg, project, command).is_some() {
        return Ok(());
    }
    let pins = declared(cfg, project)?;
    let pending: Vec<&ToolPin> = pins
        .iter()
        .filter(|pin| pin.locked.is_some() && pin.profile.is_none())
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    eprintln!(
        "`{command}` is not available yet; provisioning {} pinned tool(s) ({}) into the central tool store",
        pending.len(),
        labels(&pending).join(", ")
    );
    let report = provision_all(cfg, project, InstallMode::Symlink, false)?;
    for label in &report.provisioned {
        eprintln!("provisioned {label} in the central tool store");
    }
    for (label, error) in &report.failures {
        eprintln!("warning: could not provision tool {label}: {error}");
    }
    if let Err(error) = write_shims(cfg, project) {
        eprintln!("warning: could not refresh tool shims: {error:#}");
    }
    Ok(())
}

/// One executable a provisioned tool exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedBin {
    /// `org/name@version` of the tool that exposes it.
    pub label: String,
    /// Absolute path to the executable inside the central tool store.
    pub path: PathBuf,
}

/// Every command name this project's provisioned tools expose.
///
/// Two tools claiming one command name is a project-authoring mistake, not a
/// resolution problem: the first by package identity wins deterministically
/// and the collision is reported rather than silently resolved differently on
/// the next machine.
pub fn provisioned_bins(cfg: &Config, project: &Path) -> Result<BTreeMap<String, ProvisionedBin>> {
    let mut found: BTreeMap<String, ProvisionedBin> = BTreeMap::new();
    for pin in declared(cfg, project)? {
        let Some(profile) = &pin.profile else {
            continue;
        };
        let label = pin.label();
        let bin_dir = profile.join(MODULES_DIR).join(BIN_DIR);
        let Ok(entries) = fs::read_dir(&bin_dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.is_empty() || name.starts_with('.') {
                continue;
            }
            match found.entry(name.clone()) {
                Entry::Vacant(slot) => {
                    slot.insert(ProvisionedBin {
                        label: label.clone(),
                        path: entry.path(),
                    });
                }
                Entry::Occupied(existing) => {
                    eprintln!(
                        "warning: `{name}` is exposed by both {} and {label}; using {}",
                        existing.get().label,
                        existing.get().label
                    );
                }
            }
        }
    }
    Ok(found)
}

/// Refresh `<install.dir>/.bin` so every provisioned tool is runnable there,
/// and no shim survives its declaration.
///
/// A shim is a few hundred bytes that execs the one central copy — the whole
/// point of the design is that the tool itself is *not* here. It exists so
/// that anything already pointed at the project's bin directory (`zed run`,
/// `zed task`, a `PATH`-prepending wrapper, an editor's "use the project's
/// linter" setting) reaches the pinned version without knowing about the tool
/// store at all.
///
/// A hoisted package binary always keeps its name: a package the project
/// actually depends on outranks a tool it merely runs.
pub fn write_shims(cfg: &Config, project: &Path) -> Result<Vec<String>> {
    let bin_dir = project.join(modules_dir_name(project)).join(BIN_DIR);
    let desired = provisioned_bins(cfg, project)?;
    prune_stale_shims(&bin_dir, &desired)?;
    if desired.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(&bin_dir)?;
    let mut written = Vec::new();
    for (command, bin) in &desired {
        let hoisted = bin_dir.join(command);
        let destination = bin_dir.join(shim_file_name(command));
        if destination != hoisted && hoisted.exists() {
            continue;
        }
        // A file already at that name which zed did not write is a hoisted
        // package bin; it keeps the name.
        if fs::read(&destination).is_ok_and(|existing| !is_shim(&existing)) {
            continue;
        }
        write_shim(&destination, &bin.label, &bin.path)
            .with_context(|| format!("writing tool shim {}", destination.display()))?;
        written.push(command.clone());
    }
    Ok(written)
}

/// Remove shims zed wrote that no declared, provisioned tool still claims.
fn prune_stale_shims(bin_dir: &Path, desired: &BTreeMap<String, ProvisionedBin>) -> Result<()> {
    let Ok(entries) = fs::read_dir(bin_dir) else {
        return Ok(());
    };
    let wanted: BTreeSet<String> = desired.keys().map(|name| shim_file_name(name)).collect();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if wanted.contains(&name) {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        if is_shim(&bytes) {
            fs::remove_file(entry.path())
                .with_context(|| format!("removing stale tool shim {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Did zed write this file? Only the head is inspected: a shim is tiny, and a
/// large unrelated binary must not be read into memory to answer this.
fn is_shim(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    String::from_utf8_lossy(head).contains(SHIM_MARKER)
}

#[cfg(windows)]
fn shim_file_name(command: &str) -> String {
    format!("{command}.cmd")
}

#[cfg(not(windows))]
fn shim_file_name(command: &str) -> String {
    command.to_string()
}

fn write_shim(destination: &Path, label: &str, target: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(destination)?;
        } else {
            fs::remove_file(destination)?;
        }
    }
    fs::write(destination, shim_body(label, target))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// The shim checks its target before exec'ing so that a store pruned by
/// `zed gc`, or a lockfile pulled from a teammate whose pin this machine has
/// never fetched, produces the one instruction that fixes it — rather than a
/// bare "not found" from the shell.
#[cfg(not(windows))]
fn shim_body(label: &str, target: &Path) -> String {
    let quoted = shell_single_quote(&target.display().to_string());
    format!(
        "#!/bin/sh\n\
         # {SHIM_MARKER} {label}\n\
         # Generated by zed. Do not edit: every `zed install` rewrites this file.\n\
         target={quoted}\n\
         if [ ! -x \"$target\" ]; then\n\
         \x20 printf '%s\\n' 'zed: pinned tool {label} is not provisioned; run `zed tools sync`' >&2\n\
         \x20 exit 127\n\
         fi\n\
         exec \"$target\" \"$@\"\n"
    )
}

#[cfg(windows)]
fn shim_body(label: &str, target: &Path) -> String {
    format!(
        "@echo off\r\n\
         rem {SHIM_MARKER} {label}\r\n\
         rem Generated by zed. Do not edit: every `zed install` rewrites this file.\r\n\
         set \"ZED_TOOL_TARGET={}\"\r\n\
         if not exist \"%ZED_TOOL_TARGET%\" (\r\n\
         \x20 echo zed: pinned tool {label} is not provisioned; run `zed tools sync` 1>&2\r\n\
         \x20 exit /b 127\r\n\
         )\r\n\
         \"%ZED_TOOL_TARGET%\" %*\r\n",
        target.display()
    )
}

#[cfg(not(windows))]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The project's install directory name, honoring `[install].dir`, falling
/// back to the default when the project has no readable manifest.
fn modules_dir_name(project: &Path) -> String {
    read_project_manifest(project)
        .ok()
        .flatten()
        .map(|manifest| manifest.modules_dir().to_string())
        .unwrap_or_else(|| MODULES_DIR.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_TOOL: &str = r#"
[package]
org = "acme"
name = "consumer"
version = "0.0.0"

[package.repository]
vcs = "git"
url = "https://localhost/acme/consumer"

[tool-dependencies]
"acme/lint" = "^9"
"#;

    const WITHOUT_TOOL: &str = r#"
[package]
org = "acme"
name = "consumer"
version = "0.0.0"

[package.repository]
vcs = "git"
url = "https://localhost/acme/consumer"
"#;

    const LOCK_WITH_TOOL: &str = r#"version = 1

[[tool]]
org = "acme"
name = "lint"
version = "9.12.0"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
size = 42
format = "tar.gz"
vcs_tag = "v9.12.0"
vcs_commit = "fedcba9876543210fedcba9876543210fedcba98"
source = "file:///tmp/registry"
"#;

    fn config(home: &Path) -> Config {
        Config {
            registry: "file:///unused".to_string(),
            home: home.to_path_buf(),
            token: None,
            auth_url: "https://localhost/unused".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        }
    }

    /// A project that declares and pins `acme/lint`, with nothing provisioned.
    fn project_with_pin(dir: &Path) {
        fs::write(dir.join(MANIFEST_FILE), WITH_TOOL).unwrap();
        fs::write(dir.join(LOCKFILE_FILE), LOCK_WITH_TOOL).unwrap();
    }

    /// The on-disk result of provisioning, without the network: what
    /// `provision_pin` would leave behind for one version.
    fn provision_fake(cfg: &Config, key: &str, version: &str, bins: &[(&str, &str)]) -> PathBuf {
        let package_root = global::profile_root(cfg, key).unwrap();
        let root = global::version_dir(&package_root, version).unwrap();
        let bin_dir = root.join(MODULES_DIR).join(BIN_DIR);
        fs::create_dir_all(&bin_dir).unwrap();
        for (name, body) in bins {
            fs::write(bin_dir.join(name), body).unwrap();
        }
        global::write_profile_metadata(&root, key, &format!("{key}@={version}"), version).unwrap();
        root
    }

    fn project_bin_dir(project: &Path) -> PathBuf {
        project.join(MODULES_DIR).join(BIN_DIR)
    }

    #[test]
    fn a_declared_tool_is_pinned_and_provisioned_without_entering_the_project_tree() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        project_with_pin(project.path());
        let profile = provision_fake(
            &cfg,
            "acme/lint",
            "9.12.0",
            &[("lint", "#!/bin/sh\nexit 0\n")],
        );

        let written = write_shims(&cfg, project.path()).unwrap();
        assert_eq!(written, vec!["lint".to_string()]);

        // The whole point: the project gained a shim, not a package tree.
        assert!(!project.path().join(MODULES_DIR).join("acme").exists());
        let shim = project_bin_dir(project.path()).join(shim_file_name("lint"));
        let body = fs::read_to_string(&shim).unwrap();
        assert!(body.contains(SHIM_MARKER));
        let target = profile.join(MODULES_DIR).join(BIN_DIR).join("lint");
        assert!(
            body.contains(&target.display().to_string()),
            "the shim must exec the one central copy: {body}"
        );
        assert!(body.len() < 1024, "a shim is bytes, not a tool: {body}");
        assert_eq!(locate(&cfg, project.path(), "lint"), Some(target));
    }

    #[test]
    fn a_shim_is_pruned_when_its_declaration_goes_away() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        project_with_pin(project.path());
        provision_fake(
            &cfg,
            "acme/lint",
            "9.12.0",
            &[("lint", "#!/bin/sh\nexit 0\n")],
        );
        write_shims(&cfg, project.path()).unwrap();
        let shim = project_bin_dir(project.path()).join(shim_file_name("lint"));
        assert!(shim.exists());

        fs::write(project.path().join(MANIFEST_FILE), WITHOUT_TOOL).unwrap();
        assert!(write_shims(&cfg, project.path()).unwrap().is_empty());
        assert!(
            !shim.exists(),
            "an undeclared tool must not keep a runnable name in the project"
        );
    }

    #[test]
    fn a_hoisted_package_bin_outranks_a_declared_tool_of_the_same_name() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        project_with_pin(project.path());
        provision_fake(
            &cfg,
            "acme/lint",
            "9.12.0",
            &[("lint", "#!/bin/sh\nexit 0\n")],
        );
        let bin_dir = project_bin_dir(project.path());
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("lint"), b"hoisted package bin").unwrap();

        assert!(write_shims(&cfg, project.path()).unwrap().is_empty());
        assert_eq!(
            fs::read(bin_dir.join("lint")).unwrap(),
            b"hoisted package bin",
            "a package the project depends on is never displaced by a tool it merely runs"
        );
    }

    #[test]
    fn pruning_only_removes_files_zed_wrote() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        fs::write(project.path().join(MANIFEST_FILE), WITHOUT_TOOL).unwrap();
        let bin_dir = project_bin_dir(project.path());
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("hand-rolled"), b"#!/bin/sh\necho mine\n").unwrap();

        write_shims(&cfg, project.path()).unwrap();
        assert!(bin_dir.join("hand-rolled").exists());
    }

    #[test]
    fn two_projects_pinning_one_version_share_a_single_central_copy() {
        let home = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        project_with_pin(first.path());
        project_with_pin(second.path());
        let profile = provision_fake(
            &cfg,
            "acme/lint",
            "9.12.0",
            &[("lint", "#!/bin/sh\nexit 0\n")],
        );

        write_shims(&cfg, first.path()).unwrap();
        write_shims(&cfg, second.path()).unwrap();

        let target = profile.join(MODULES_DIR).join(BIN_DIR).join("lint");
        assert_eq!(locate(&cfg, first.path(), "lint").as_ref(), Some(&target));
        assert_eq!(locate(&cfg, second.path(), "lint").as_ref(), Some(&target));
        // The npm failure mode this whole feature exists to avoid.
        let versions = fs::read_dir(global::profile_root(&cfg, "acme/lint").unwrap())
            .unwrap()
            .count();
        assert_eq!(versions, 1);
    }

    #[test]
    fn an_unpinned_declaration_is_reported_rather_than_aborting_the_pass() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        fs::write(project.path().join(MANIFEST_FILE), WITH_TOOL).unwrap();

        let report = provision_all(&cfg, project.path(), InstallMode::Symlink, false).unwrap();
        assert!(report.provisioned.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].1.contains(LOCKFILE_FILE));
    }

    #[test]
    fn the_install_summary_reports_pins_without_pre_empting_provisioning() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        project_with_pin(project.path());

        let pending = install_summary(&cfg, project.path()).unwrap();
        assert!(pending.contains("acme/lint@9.12.0"), "{pending}");
        assert!(
            !pending.contains("zed tools sync"),
            "the CLI provisions two lines later; do not send the reader elsewhere: {pending}"
        );

        provision_fake(
            &cfg,
            "acme/lint",
            "9.12.0",
            &[("lint", "#!/bin/sh\nexit 0\n")],
        );
        let ready = install_summary(&cfg, project.path()).unwrap();
        assert!(
            ready.contains("all present in the central tool store"),
            "{ready}"
        );
    }
}
