//! Credential-free, non-mutating project diagnostics for editor integrations.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value as JsonValue;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};
use zed_interfaces::{Lockfile, Manifest};

use crate::{environment, transaction};

pub const INSPECTION_SCHEMA_V1: &str = "1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InspectFormat {
    Json,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub schema_version: &'static str,
    pub producer: Producer,
    pub root: PathBuf,
    pub package: PackageSnapshot,
    pub interop: InteropSnapshot,
    pub summary: Summary,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Producer {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageSnapshot {
    pub manifest: Option<PathBuf>,
    pub lockfile: Option<PathBuf>,
    pub materialization_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct InteropSnapshot {
    pub git_submodules: InteropStatus,
    pub mise: InteropStatus,
    pub nix_develop: InteropStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct InteropStatus {
    pub detected: bool,
    pub enabled: bool,
    pub ready: bool,
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub health: Severity,
    pub errors: usize,
    pub warnings: usize,
    pub infos: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub id: &'static str,
    pub title: &'static str,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub mutates_project: bool,
    pub requires_network: bool,
    pub executes_package_code: bool,
}

impl Inspection {
    fn finish(mut self) -> Self {
        self.diagnostics.sort_by(|left, right| {
            left.code.cmp(right.code).then_with(|| {
                left.location
                    .as_ref()
                    .map(|location| &location.path)
                    .cmp(&right.location.as_ref().map(|location| &location.path))
            })
        });
        let errors = self
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
            .count();
        let warnings = self
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Warning)
            .count();
        let infos = self
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Info)
            .count();
        let health = if errors > 0 {
            Severity::Error
        } else if warnings > 0 {
            Severity::Warning
        } else if infos > 0 {
            Severity::Info
        } else {
            Severity::Ok
        };
        self.summary = Summary {
            health,
            errors,
            warnings,
            infos,
        };
        self
    }
}

pub fn run(root: Option<&Path>, format: InspectFormat) -> Result<()> {
    let requested = root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let inspection = inspect(&requested);
    match format {
        InspectFormat::Json => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            serde_json::to_writer(&mut output, &inspection)
                .context("serializing project inspection")?;
            output
                .write_all(b"\n")
                .context("writing project inspection")?;
        }
    }
    Ok(())
}

pub fn inspect(requested: &Path) -> Inspection {
    let absolute = absolute_path(requested);
    let mut inspection = empty_inspection(absolute.clone());
    let root = match absolute.canonicalize() {
        Ok(path) if path.is_dir() => path,
        Ok(path) => {
            inspection.diagnostics.push(diagnostic(
                "ROOT_NOT_DIRECTORY",
                Severity::Error,
                "Inspection root is not a directory",
                "Choose a project directory containing .zpkg.toml or related interoperability files.",
                Some(path),
            ));
            return inspection.finish();
        }
        Err(_) => {
            inspection.diagnostics.push(diagnostic(
                "ROOT_UNAVAILABLE",
                Severity::Error,
                "Inspection root could not be resolved",
                "The requested path does not exist or is not accessible.",
                Some(absolute),
            ));
            return inspection.finish();
        }
    };

    inspection.root = root.clone();
    let manifest_path = root.join(MANIFEST_FILE);
    let lock_path = root.join(LOCKFILE_FILE);
    let manifest_present = manifest_path.is_file();
    let lock_present = lock_path.is_file();
    inspection.package.manifest = manifest_present.then(|| manifest_path.clone());
    inspection.package.lockfile = lock_present.then(|| lock_path.clone());

    let mut manifest = None;
    let mut raw_manifest = None;
    if manifest_present {
        match read_utf8(&manifest_path) {
            Ok(text) => {
                raw_manifest = toml::from_str::<toml::Value>(&text).ok();
                match Manifest::parse(&text) {
                    Ok(parsed) => manifest = Some(parsed),
                    Err(_) => inspection.diagnostics.push(diagnostic(
                        "MANIFEST_INVALID",
                        Severity::Error,
                        ".zpkg.toml is invalid",
                        "The manifest could not be parsed or validated by the current Zed contract.",
                        Some(manifest_path.clone()),
                    )),
                }
            }
            Err(_) => inspection.diagnostics.push(diagnostic(
                "MANIFEST_UNREADABLE",
                Severity::Error,
                ".zpkg.toml is not readable UTF-8",
                "Editor diagnostics require a local UTF-8 manifest.",
                Some(manifest_path.clone()),
            )),
        }
    } else if lock_present {
        inspection.diagnostics.push(diagnostic(
            "LOCK_WITHOUT_MANIFEST",
            Severity::Warning,
            "Lock-only Zed project",
            "Use the explicit lock-only frozen restore mode; the lock cannot identify direct dependency intent.",
            Some(lock_path.clone()),
        ));
    }

    if let Some(manifest) = &manifest {
        inspection.package.materialization_dir = root.join(manifest.modules_dir());
    }

    let mut lock = None;
    let mut raw_lock = None;
    if lock_present {
        match read_utf8(&lock_path) {
            Ok(text) => {
                raw_lock = toml::from_str::<toml::Value>(&text).ok();
                match Lockfile::parse(&text) {
                    Ok(parsed) => lock = Some(parsed),
                    Err(_) => inspection.diagnostics.push(diagnostic(
                        "LOCK_INVALID",
                        Severity::Error,
                        ".zpkg.lock is invalid",
                        "The lockfile could not be parsed or validated by the current Zed contract.",
                        Some(lock_path.clone()),
                    )),
                }
            }
            Err(_) => inspection.diagnostics.push(diagnostic(
                "LOCK_UNREADABLE",
                Severity::Error,
                ".zpkg.lock is not readable UTF-8",
                "Editor diagnostics require a local UTF-8 lockfile.",
                Some(lock_path.clone()),
            )),
        }
    }

    if manifest_present && !lock_present {
        inspection.diagnostics.push(diagnostic(
            "LOCK_MISSING",
            Severity::Warning,
            "Manifest has no Zed lockfile",
            "Run zed install before relying on frozen or reproducible editor tooling.",
            Some(manifest_path.clone()),
        ));
    } else if manifest_present && lock_present && is_newer(&manifest_path, &lock_path) {
        inspection.diagnostics.push(diagnostic(
            "LOCK_STALE",
            Severity::Warning,
            "Manifest is newer than the Zed lockfile",
            "Resolve again before relying on the locked graph.",
            Some(manifest_path.clone()),
        ));
    }

    let has_dependency_intent = manifest.as_ref().is_some_and(|manifest| {
        !manifest.dependencies.is_empty() || !manifest.build_dependencies.is_empty()
    }) || lock.as_ref().is_some_and(|lock| !lock.packages.is_empty());
    if lock_present && has_dependency_intent && !inspection.package.materialization_dir.exists() {
        inspection.diagnostics.push(diagnostic(
            "MATERIALIZATION_MISSING",
            Severity::Warning,
            "Pinned dependencies are not materialized",
            "The Zed lock exists, but the configured dependency directory does not.",
            Some(inspection.package.materialization_dir.clone()),
        ));
    }

    let staging = root.join(transaction::STAGING_DIR);
    if staging.is_dir() && directory_has_entries(&staging) {
        inspection.diagnostics.push(diagnostic(
            "TRANSACTION_INTERRUPTED",
            Severity::Error,
            "Interrupted Zed transaction needs recovery",
            "A lifecycle command must recover the transaction before new work starts.",
            Some(staging),
        ));
    }

    inspect_git_submodules(
        &root,
        &manifest_path,
        raw_manifest.as_ref(),
        raw_lock.as_ref(),
        &mut inspection,
    );
    let mise_ready = inspect_mise(&root, &mut inspection);
    let nix_ready = inspect_nix(&root, &mut inspection);
    if mise_ready && nix_ready {
        inspection.diagnostics.push(diagnostic(
            "ENVIRONMENT_LAYERED",
            Severity::Info,
            "Nix and mise development environments are both active contracts",
            "zed develop composes nix develop as the outer environment and project-local mise as the inner tool layer.",
            Some(root),
        ));
    }

    inspection.finish()
}

fn empty_inspection(root: PathBuf) -> Inspection {
    Inspection {
        schema_version: INSPECTION_SCHEMA_V1,
        producer: Producer {
            name: "zed-pkg",
            version: env!("CARGO_PKG_VERSION"),
        },
        package: PackageSnapshot {
            manifest: None,
            lockfile: None,
            materialization_dir: root.join(MODULES_DIR),
        },
        interop: InteropSnapshot {
            git_submodules: empty_interop(),
            mise: empty_interop(),
            nix_develop: empty_interop(),
        },
        root,
        summary: Summary {
            health: Severity::Ok,
            errors: 0,
            warnings: 0,
            infos: 0,
        },
        diagnostics: Vec::new(),
    }
}

fn empty_interop() -> InteropStatus {
    InteropStatus {
        detected: false,
        enabled: false,
        ready: false,
        files: Vec::new(),
    }
}

fn inspect_git_submodules(
    root: &Path,
    manifest_path: &Path,
    manifest: Option<&toml::Value>,
    lock: Option<&toml::Value>,
    inspection: &mut Inspection,
) {
    let gitmodules = root.join(".gitmodules");
    let detected = fs::symlink_metadata(&gitmodules).is_ok();
    let (enabled, flag_valid) = manifest_git_submodules(manifest);
    let lock_entries = lock
        .and_then(|value| value.get("git-submodule"))
        .and_then(toml::Value::as_array)
        .map_or(0, Vec::len);

    inspection.interop.git_submodules = InteropStatus {
        detected,
        enabled,
        ready: false,
        files: detected.then(|| gitmodules.clone()).into_iter().collect(),
    };

    if !flag_valid {
        inspection.diagnostics.push(diagnostic(
            "GITMODULES_FLAG_INVALID",
            Severity::Error,
            "The Git-submodule interop flag is not boolean",
            "Use `[interop] git-submodules = true` or remove the key.",
            Some(manifest_path.to_path_buf()),
        ));
        return;
    }

    if lock_entries > 0 && !enabled {
        inspection.diagnostics.push(diagnostic(
            "GIT_SUBMODULE_LOCK_UNCLAIMED",
            Severity::Error,
            "The Zed lock contains Git-submodule provenance without manifest opt-in",
            "Declare `[interop] git-submodules = true` or regenerate the lock without adopted submodule entries.",
            Some(manifest_path.to_path_buf()),
        ));
    }

    match (detected, enabled) {
        (true, false) => inspection.diagnostics.push(Diagnostic {
            code: "GITMODULES_UNCLAIMED",
            severity: Severity::Warning,
            message: ".gitmodules is present but not consumed by Zed".to_string(),
            detail: "Add `[interop] git-submodules = true` only when Zed should synchronize and verify this metadata.".to_string(),
            location: Some(location(gitmodules)),
            actions: vec![open_action(
                "open-manifest-for-git-interop",
                "Review the Zed manifest interop declaration",
                manifest_path.to_path_buf(),
            )],
        }),
        (false, true) => inspection.diagnostics.push(diagnostic(
            "GITMODULES_REQUIRED",
            Severity::Error,
            "Git-submodule interop is enabled but .gitmodules is missing",
            "Restore committed .gitmodules metadata or disable the manifest opt-in.",
            Some(manifest_path.to_path_buf()),
        )),
        (true, true) => match fs::symlink_metadata(&gitmodules) {
            Ok(metadata) if metadata.file_type().is_file() => {
                inspection.interop.git_submodules.ready = true;
            }
            _ => inspection.diagnostics.push(diagnostic(
                "GITMODULES_INVALID",
                Severity::Error,
                ".gitmodules is not safe static input",
                "Zed requires .gitmodules to be a regular file. Runtime commands additionally verify committed Git provenance before consuming it.",
                Some(gitmodules),
            )),
        },
        (false, false) => {}
    }
}

fn inspect_mise(root: &Path, inspection: &mut Inspection) -> bool {
    let candidates = [root.join("mise.toml"), root.join(".mise.toml")];
    let configs = candidates
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    let tool_versions = root.join(".tool-versions");
    let lock = root.join("mise.lock");
    let mut files = configs.clone();
    if lock.is_file() {
        files.push(lock.clone());
    }
    if tool_versions.is_file() {
        files.push(tool_versions.clone());
    }
    inspection.interop.mise = InteropStatus {
        detected: !files.is_empty(),
        enabled: !configs.is_empty(),
        ready: false,
        files,
    };

    if configs.len() > 1 {
        inspection.diagnostics.push(diagnostic(
            "MISE_CONFIG_AMBIGUOUS",
            Severity::Error,
            "Both mise.toml and .mise.toml are present",
            "Keep one canonical project-local mise configuration so editors and mise resolve the same source.",
            Some(root.to_path_buf()),
        ));
        return false;
    }
    let Some(config) = configs.first() else {
        if tool_versions.is_file() {
            inspection.diagnostics.push(diagnostic(
                "MISE_TOOL_VERSIONS_UNCONSUMED",
                Severity::Warning,
                ".tool-versions is present without a project-local mise TOML configuration",
                "zed develop intentionally does not import ambient or legacy tool versions in frozen mode; add mise.toml or .mise.toml for explicit interop.",
                Some(tool_versions),
            ));
        }
        return false;
    };

    if environment::import_mise(root, Some(config), None, false).is_err() {
        inspection.diagnostics.push(diagnostic(
            "MISE_CONFIG_INVALID",
            Severity::Error,
            "The project-local mise configuration is not statically compatible",
            "Zed accepts the deterministic tools/settings subset and never executes mise hooks during inspection.",
            Some(config.clone()),
        ));
        return false;
    }
    if !lock.is_file() {
        inspection.diagnostics.push(diagnostic(
            "MISE_LOCK_MISSING",
            Severity::Warning,
            "The mise configuration has no adjacent mise.lock",
            "Commit mise.lock before using frozen editor, CI, or zed develop composition.",
            Some(config.clone()),
        ));
        return false;
    }
    if environment::import_mise(root, Some(config), Some(&lock), true).is_err() {
        inspection.diagnostics.push(diagnostic(
            "MISE_FROZEN_UNREADY",
            Severity::Error,
            "mise.lock is not ready for frozen portable replay",
            "Lock coverage, exact versions, supported settings, platforms, and cryptographic artifact checksums must all validate.",
            Some(lock),
        ));
        return false;
    }

    inspection.interop.mise.ready = true;
    true
}

fn inspect_nix(root: &Path, inspection: &mut Inspection) -> bool {
    let nested_dir = root.join(".nix");
    let nested_flake = nested_dir.join("flake.nix");
    let nested_lock = nested_dir.join("flake.lock");
    let root_flake = root.join("flake.nix");
    let root_lock = root.join("flake.lock");
    let envrc = root.join(".envrc");
    let nested_enabled = nested_flake.is_file();
    let root_enabled = root_flake.is_file();
    let (flake, lock) = if nested_enabled {
        (nested_flake.clone(), nested_lock.clone())
    } else {
        (root_flake.clone(), root_lock.clone())
    };
    let detected = nested_enabled
        || root_enabled
        || nested_lock.is_file()
        || root_lock.is_file()
        || envrc_uses_flake(&envrc);
    let mut files = Vec::new();
    for path in [&nested_flake, &nested_lock, &root_flake, &root_lock, &envrc] {
        if path.is_file() {
            files.push(path.clone());
        }
    }
    inspection.interop.nix_develop = InteropStatus {
        detected,
        enabled: flake.is_file(),
        ready: false,
        files,
    };

    if nested_enabled && root_enabled {
        inspection.diagnostics.push(diagnostic(
            "NIX_FLAKE_SHADOWED",
            Severity::Warning,
            "The root flake is shadowed by .nix/flake.nix",
            "zed develop follows its runtime precedence and selects .nix/flake.nix. Remove the duplicate or keep both lock graphs intentionally aligned.",
            Some(root_flake.clone()),
        ));
    }
    for (candidate_flake, candidate_lock) in
        [(&nested_flake, &nested_lock), (&root_flake, &root_lock)]
    {
        if candidate_lock.is_file() && !candidate_flake.is_file() {
            inspection.diagnostics.push(diagnostic(
                "NIX_LOCK_WITHOUT_FLAKE",
                Severity::Error,
                "flake.lock is present without its adjacent flake.nix",
                "nix develop cannot resolve a lock graph without the flake entrypoint in the same directory.",
                Some(candidate_lock.clone()),
            ));
        }
    }
    if !flake.is_file() {
        if envrc_uses_flake(&envrc) {
            inspection.diagnostics.push(diagnostic(
                "NIX_ENVRC_MISSING_FLAKE",
                Severity::Warning,
                ".envrc requests a flake that is not present",
                "Restore flake.nix or update the direnv contract so editor activation agrees with nix develop.",
                Some(envrc),
            ));
        }
        return false;
    }
    if read_utf8(&flake).is_err() {
        inspection.diagnostics.push(diagnostic(
            "NIX_FLAKE_UNREADABLE",
            Severity::Error,
            "flake.nix is not readable UTF-8",
            "Static editor analysis requires a local text flake entrypoint.",
            Some(flake),
        ));
        return false;
    }
    if !lock.is_file() {
        inspection.diagnostics.push(diagnostic(
            "NIX_LOCK_MISSING",
            Severity::Warning,
            "flake.nix has no flake.lock",
            "Generate and commit flake.lock before relying on reproducible nix develop composition.",
            Some(flake),
        ));
        return false;
    }
    if validate_flake_lock(&lock).is_err() {
        inspection.diagnostics.push(diagnostic(
            "NIX_LOCK_INVALID",
            Severity::Error,
            "flake.lock is not a valid Nix lock graph",
            "The lock must be UTF-8 JSON with a positive version, a root node name, and a nodes object containing that root.",
            Some(lock),
        ));
        return false;
    }

    inspection.interop.nix_develop.ready = true;
    true
}

fn manifest_git_submodules(manifest: Option<&toml::Value>) -> (bool, bool) {
    let Some(interop) = manifest
        .and_then(|value| value.get("interop"))
        .and_then(toml::Value::as_table)
    else {
        return (false, true);
    };
    match interop.get("git-submodules") {
        None => (false, true),
        Some(value) => match value.as_bool() {
            Some(enabled) => (enabled, true),
            None => (false, false),
        },
    }
}

fn validate_flake_lock(path: &Path) -> Result<()> {
    let text = read_utf8(path)?;
    let value: JsonValue = serde_json::from_str(&text).context("parsing flake.lock JSON")?;
    let object = value.as_object().context("flake.lock must be an object")?;
    let version = object
        .get("version")
        .and_then(JsonValue::as_u64)
        .filter(|version| *version > 0)
        .context("flake.lock has no positive version")?;
    let _ = version;
    let root = object
        .get("root")
        .and_then(JsonValue::as_str)
        .filter(|root| !root.is_empty())
        .context("flake.lock has no root node")?;
    let nodes = object
        .get("nodes")
        .and_then(JsonValue::as_object)
        .context("flake.lock has no nodes object")?;
    nodes
        .get(root)
        .and_then(JsonValue::as_object)
        .context("flake.lock root does not exist in nodes")?;
    Ok(())
}

fn envrc_uses_flake(path: &Path) -> bool {
    read_utf8(path).is_ok_and(|text| {
        text.lines().any(|line| {
            let line = line.trim();
            !line.starts_with('#')
                && (line == "use flake"
                    || line.starts_with("use flake ")
                    || line.starts_with("use_flake"))
        })
    })
}

fn read_utf8(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn is_newer(first: &Path, second: &Path) -> bool {
    let Ok(first) = first.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let Ok(second) = second.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    first
        .duration_since(second)
        .is_ok_and(|duration| duration.as_secs_f64() > 1.0)
}

fn directory_has_entries(path: &Path) -> bool {
    fs::read_dir(path)
        .ok()
        .and_then(|mut entries| entries.next())
        .is_some()
}

fn diagnostic(
    code: &'static str,
    severity: Severity,
    message: &str,
    detail: &str,
    path: Option<PathBuf>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity,
        message: message.to_string(),
        detail: detail.to_string(),
        location: path.map(location),
        actions: Vec::new(),
    }
}

fn location(path: PathBuf) -> Location {
    Location {
        path,
        line: None,
        column: None,
    }
}

fn open_action(id: &'static str, title: &'static str, path: PathBuf) -> Action {
    Action {
        id,
        title,
        kind: "open-file",
        argv: Vec::new(),
        path: Some(path),
        mutates_project: false,
        requires_network: false,
        executes_package_code: false,
    }
}
