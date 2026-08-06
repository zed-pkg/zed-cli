//! Read-only workspace inspection for IDEs, agents, and CI annotations.
//!
//! This module intentionally performs no recovery, resolution, downloads,
//! credential loading, hook execution, or project writes. Network-backed
//! version recommendations are an explicit opt-in and use public package
//! metadata only.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use semver::Version;
use serde::Serialize;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::{Lockfile, Manifest};

use crate::config::Config;
use crate::registry::registry_for;
use crate::store::Store;

pub const INSPECTION_SCHEMA_VERSION: u32 = 1;
const MAX_INSPECTION_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectionReport {
    pub schema_version: u32,
    pub zed_version: String,
    pub workspace_root: String,
    pub package: PackageInspection,
    pub interop: InteropInspection,
    pub network: NetworkInspection,
    pub updates: Vec<VersionRecommendation>,
    pub summary: InspectionSummary,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInspection {
    pub identity: Option<String>,
    pub version: Option<String>,
    pub manifest_path: String,
    pub lock_path: String,
    pub materialization_dir: String,
    pub manifest_valid: bool,
    pub lock_valid: bool,
    pub frozen_ready: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InteropInspection {
    pub git_submodules: InteropStatus,
    pub mise: InteropStatus,
    pub nix_develop: InteropStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InteropStatus {
    pub detected: bool,
    pub declared: bool,
    pub verified: bool,
    pub source: Option<String>,
}

impl InteropStatus {
    fn absent() -> Self {
        Self {
            detected: false,
            declared: false,
            verified: false,
            source: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInspection {
    pub enabled: bool,
    pub registry: Option<String>,
    pub update_check_complete: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionRecommendation {
    pub package: String,
    pub current: String,
    pub latest: String,
    pub change: VersionChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionChange {
    Major,
    Minor,
    Patch,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectionSummary {
    pub health: Health,
    pub errors: usize,
    pub warnings: usize,
    pub information: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Healthy,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub location: Option<DiagnosticLocation>,
    pub actions: Vec<RecommendedAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticLocation {
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecommendedAction {
    pub id: String,
    pub title: String,
    pub kind: ActionKind,
    pub argv: Vec<String>,
    pub working_directory: String,
    pub mutates_project: bool,
    pub requires_network: bool,
    pub executes_package_code: bool,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    ZedCommand,
    ExternalCommand,
    EditFile,
}

pub fn inspect(requested: &Path, cfg: &Config, check_updates: bool) -> Result<InspectionReport> {
    let root = absolute_workspace(requested)?;
    let root_text = path_text(&root);
    let manifest_path = root.join(MANIFEST_FILE);
    let lock_path = root.join(LOCKFILE_FILE);
    let mut diagnostics = Vec::new();
    let mut manifest = None;
    let mut lock = None;

    if !root.is_dir() {
        diagnostics.push(diagnostic(
            "workspace.invalid",
            Severity::Error,
            "Workspace is unavailable",
            "The requested workspace must be an existing directory.",
            Some(&root),
            Vec::new(),
        ));
    } else if !manifest_path.exists() {
        diagnostics.push(diagnostic(
            "manifest.missing",
            Severity::Warning,
            "Zed manifest is missing",
            "This workspace has no .zpkg.toml manifest.",
            Some(&manifest_path),
            vec![zed_action(
                &root,
                "initialize-manifest",
                "Initialize a Zed package",
                &["zed", "init"],
                true,
                false,
                false,
            )],
        ));
    } else if !is_regular_file(&manifest_path) {
        diagnostics.push(diagnostic(
            "manifest.not-regular",
            Severity::Error,
            "Zed manifest is not a regular file",
            "Refusing to inspect an indirect or non-regular manifest.",
            Some(&manifest_path),
            Vec::new(),
        ));
    } else {
        match read_bounded_text(&manifest_path)
            .context("reading manifest")
            .and_then(|text| Manifest::parse(&text).context("parsing manifest"))
        {
            Ok(parsed) => manifest = Some(parsed),
            Err(_) => diagnostics.push(diagnostic(
                "manifest.invalid",
                Severity::Error,
                "Zed manifest is invalid",
                "The manifest could not be decoded using the current zed-interfaces contract.",
                Some(&manifest_path),
                Vec::new(),
            )),
        }
    }

    if lock_path.exists() && !is_regular_file(&lock_path) {
        diagnostics.push(diagnostic(
            "lock.not-regular",
            Severity::Error,
            "Zed lockfile is not a regular file",
            "Refusing to inspect an indirect or non-regular lockfile.",
            Some(&lock_path),
            Vec::new(),
        ));
    } else if lock_path.is_file() {
        match read_bounded_text(&lock_path)
            .context("reading lockfile")
            .and_then(|text| Lockfile::parse(&text).context("parsing lockfile"))
        {
            Ok(parsed) => lock = Some(parsed),
            Err(_) => diagnostics.push(diagnostic(
                "lock.invalid",
                Severity::Error,
                "Zed lockfile is invalid",
                "The lockfile could not be decoded using the current zed-interfaces contract.",
                Some(&lock_path),
                vec![zed_action(
                    &root,
                    "regenerate-lock",
                    "Regenerate the lockfile",
                    &["zed", "install"],
                    true,
                    true,
                    false,
                )],
            )),
        }
    } else if manifest
        .as_ref()
        .is_some_and(|value| !value.dependencies.is_empty())
    {
        diagnostics.push(diagnostic(
            "lock.missing",
            Severity::Warning,
            "Zed lockfile is missing",
            "The manifest declares dependencies but .zpkg.lock does not exist.",
            Some(&lock_path),
            vec![zed_action(
                &root,
                "resolve-lock",
                "Resolve and install dependencies",
                &["zed", "install"],
                true,
                true,
                false,
            )],
        ));
    }

    let modules_dir = manifest
        .as_ref()
        .map(|value| value.modules_dir().to_string())
        .unwrap_or_else(|| "zed_modules".to_string());
    let materialization = root.join(&modules_dir);
    inspect_package_state(
        &root,
        &materialization,
        manifest.as_ref(),
        lock.as_ref(),
        cfg,
        &mut diagnostics,
    );

    if root.join(crate::transaction::STAGING_DIR).exists() {
        diagnostics.push(diagnostic(
            "transaction.interrupted",
            Severity::Error,
            "An interrupted Zed transaction needs recovery",
            "Inspection left the recovery journal untouched; run a mutating Zed lifecycle command to recover it under the install lock.",
            Some(&root.join(crate::transaction::STAGING_DIR)),
            vec![zed_action(
                &root,
                "recover-install",
                "Recover with a frozen install",
                &["zed", "install", "--frozen"],
                true,
                false,
                false,
            )],
        ));
    }

    let git_submodules = inspect_git_submodules(&root, &manifest_path, &mut diagnostics);
    let mise = inspect_mise(&root, &mut diagnostics);
    let nix_develop = inspect_nix(&root, &mut diagnostics);

    let mut updates = Vec::new();
    let update_check_complete = if check_updates {
        inspect_updates(
            &root,
            cfg,
            manifest.as_ref(),
            lock.as_ref(),
            &mut updates,
            &mut diagnostics,
        )
    } else {
        false
    };

    diagnostics.sort_by(|left, right| {
        (&left.code, location_path(left), &left.title).cmp(&(
            &right.code,
            location_path(right),
            &right.title,
        ))
    });
    updates.sort_by(|left, right| left.package.cmp(&right.package));
    let summary = summarize(&diagnostics);
    let frozen_ready = manifest.is_some()
        && lock.is_some()
        && !diagnostics.iter().any(|item| {
            item.severity == Severity::Error
                || matches!(
                    item.code.as_str(),
                    "lock.missing"
                        | "lock.dependency-missing"
                        | "materialization.missing"
                        | "materialization.package-missing"
                        | "store.entry-missing"
                )
        })
        && [&git_submodules, &mise, &nix_develop]
            .into_iter()
            .all(|status| !status.detected || (status.declared && status.verified));

    Ok(InspectionReport {
        schema_version: INSPECTION_SCHEMA_VERSION,
        zed_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace_root: root_text,
        package: PackageInspection {
            identity: manifest.as_ref().map(Manifest::full_name),
            version: manifest.as_ref().map(|value| value.package.version.clone()),
            manifest_path: path_text(&manifest_path),
            lock_path: path_text(&lock_path),
            materialization_dir: path_text(&materialization),
            manifest_valid: manifest.is_some(),
            lock_valid: lock.is_some(),
            frozen_ready,
        },
        interop: InteropInspection {
            git_submodules,
            mise,
            nix_develop,
        },
        network: NetworkInspection {
            enabled: check_updates,
            registry: check_updates.then(|| cfg.registry.clone()),
            update_check_complete,
        },
        updates,
        summary,
        diagnostics,
    })
}

pub fn print(report: &InspectionReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(report)?);
        return Ok(());
    }
    println!(
        "{}: {:?} ({} error(s), {} warning(s))",
        report.workspace_root,
        report.summary.health,
        report.summary.errors,
        report.summary.warnings
    );
    for item in &report.diagnostics {
        println!("{:?} {}: {}", item.severity, item.code, item.title);
    }
    for update in &report.updates {
        println!(
            "update {:?}: {} {} -> {}",
            update.change, update.package, update.current, update.latest
        );
    }
    Ok(())
}

fn inspect_package_state(
    root: &Path,
    materialization: &Path,
    manifest: Option<&Manifest>,
    lock: Option<&Lockfile>,
    cfg: &Config,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let (Some(manifest), Some(lock)) = (manifest, lock) else {
        return;
    };

    for dependency in manifest.dependencies.keys() {
        let Some((org, name)) = dependency.split_once('/') else {
            continue;
        };
        if lock.find(org, name).is_none() {
            diagnostics.push(diagnostic(
                "lock.dependency-missing",
                Severity::Error,
                "A direct dependency is absent from the lockfile",
                &format!("{dependency} is declared by the manifest but has no lock entry."),
                Some(&root.join(MANIFEST_FILE)),
                vec![zed_action(
                    root,
                    "refresh-lock",
                    "Refresh the lockfile",
                    &["zed", "install"],
                    true,
                    true,
                    false,
                )],
            ));
        }
    }

    if !lock.packages.is_empty() && !materialization.is_dir() {
        diagnostics.push(diagnostic(
            "materialization.missing",
            Severity::Warning,
            "Installed dependency tree is missing",
            "The lockfile contains packages but the configured installation directory is absent.",
            Some(materialization),
            vec![zed_action(
                root,
                "restore-frozen",
                "Restore the frozen dependency graph",
                &["zed", "install", "--frozen"],
                true,
                false,
                false,
            )],
        ));
    }

    let store = Store::new(&cfg.home);
    for package in &lock.packages {
        let installed = materialization.join(&package.org).join(&package.name);
        if materialization.is_dir() && !installed.exists() {
            diagnostics.push(diagnostic(
                "materialization.package-missing",
                Severity::Warning,
                "A locked package is not materialized",
                &format!(
                    "{} is locked but missing from the installed tree.",
                    package.full_name()
                ),
                Some(&installed),
                vec![zed_action(
                    root,
                    "restore-frozen",
                    "Restore the frozen dependency graph",
                    &["zed", "install", "--frozen"],
                    true,
                    false,
                    false,
                )],
            ));
        }
        if !store.has(&package.sha256) {
            diagnostics.push(diagnostic(
                "store.entry-missing",
                Severity::Warning,
                "A locked artifact is absent from the local store",
                &format!(
                    "{} must be fetched before an offline frozen restore.",
                    package.full_name()
                ),
                Some(&store.pkg_dir(&package.sha256)),
                vec![zed_action(
                    root,
                    "fetch-frozen",
                    "Fetch frozen artifacts",
                    &["zed", "fetch", "--frozen"],
                    true,
                    true,
                    false,
                )],
            ));
        }
    }
}

fn inspect_git_submodules(
    root: &Path,
    manifest_path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> InteropStatus {
    let (declared, declaration_valid) =
        match crate::git_submodules::manifest_consumes_gitmodules(root) {
            Ok(declared) => (declared, true),
            Err(error) => {
                diagnostics.push(diagnostic(
                    "interop.git-submodules.declaration-invalid",
                    Severity::Error,
                    "Git-submodule consumption declaration is invalid",
                    error.to_string(),
                    Some(manifest_path),
                    Vec::new(),
                ));
                (false, false)
            }
        };
    let discovered_root = crate::git_submodules::find_root(root);
    let detected = discovered_root.is_some();
    let source = discovered_root
        .as_ref()
        .map(|path| path_text(&path.join(".gitmodules")));
    let mut verified = false;

    if declared && !detected {
        diagnostics.push(diagnostic(
            "interop.git-submodules.missing",
            Severity::Error,
            "Git-submodule consumption is declared without .gitmodules",
            "[interop.git].consume_gitmodules is true, but no .gitmodules file was found inside the owning checkout.",
            Some(manifest_path),
            Vec::new(),
        ));
    } else if let Some(project) = discovered_root {
        match crate::git_submodules::preflight_gitmodules_metadata(&project) {
            Ok(()) => verified = true,
            Err(_) => diagnostics.push(diagnostic(
                "interop.git-submodules.invalid",
                Severity::Error,
                "Git submodule metadata failed static validation",
                ".gitmodules must be a committed regular file with safe, unambiguous index metadata.",
                Some(&project.join(".gitmodules")),
                Vec::new(),
            )),
        }
        if !declared {
            diagnostics.push(diagnostic(
                "interop.git-submodules.undeclared",
                Severity::Warning,
                "Declare Zed consumption of .gitmodules",
                "The checkout contains .gitmodules, but .zpkg.toml does not explicitly opt into Zed consumption with [interop.git].consume_gitmodules = true.",
                Some(manifest_path),
                vec![RecommendedAction {
                    id: "declare-git-submodule-consumption".to_string(),
                    title: "Add the Git interop declaration".to_string(),
                    kind: ActionKind::EditFile,
                    argv: Vec::new(),
                    working_directory: path_text(root),
                    mutates_project: true,
                    requires_network: false,
                    executes_package_code: false,
                    requires_confirmation: true,
                }],
            ));
        }
    }

    InteropStatus {
        detected,
        declared,
        verified: verified && declaration_valid,
        source,
    }
}

fn inspect_mise(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> InteropStatus {
    let candidates = ["mise.toml", ".mise.toml", ".tool-versions"];
    let present: Vec<PathBuf> = candidates
        .iter()
        .map(|name| root.join(name))
        .filter(|path| path.exists())
        .collect();
    if present.is_empty() {
        return InteropStatus::absent();
    }

    let source = present.first().map(|path| path_text(path));
    if present.len() > 1 {
        diagnostics.push(diagnostic(
            "interop.mise.ambiguous",
            Severity::Error,
            "Multiple project-local mise configurations are present",
            "Pass an explicit mise configuration; static analysis will not guess precedence.",
            present.first().map(PathBuf::as_path),
            Vec::new(),
        ));
        return InteropStatus {
            detected: true,
            declared: true,
            verified: false,
            source,
        };
    }

    let verified = crate::environment::import_mise(root, None, None, true).is_ok();
    if !verified {
        diagnostics.push(diagnostic(
            "interop.mise.unverified",
            Severity::Warning,
            "mise configuration is not frozen-portable",
            "The project-local mise configuration and lock did not satisfy Zed's read-only frozen verification contract.",
            present.first().map(PathBuf::as_path),
            vec![RecommendedAction {
                id: "refresh-mise-lock".to_string(),
                title: "Refresh and verify the mise lock".to_string(),
                kind: ActionKind::ExternalCommand,
                argv: vec!["mise".to_string(), "lock".to_string()],
                working_directory: path_text(root),
                mutates_project: true,
                requires_network: true,
                executes_package_code: false,
                requires_confirmation: true,
            }],
        ));
    }
    InteropStatus {
        detected: true,
        declared: true,
        verified,
        source,
    }
}

fn inspect_nix(root: &Path, diagnostics: &mut Vec<Diagnostic>) -> InteropStatus {
    let flake = [root.join(".nix/flake.nix"), root.join("flake.nix")]
        .into_iter()
        .find(|path| path.exists());
    let Some(flake) = flake else {
        return InteropStatus::absent();
    };
    let lock = flake.parent().unwrap_or(root).join("flake.lock");
    let mut verified = false;

    if !is_regular_file(&flake) {
        diagnostics.push(diagnostic(
            "interop.nix.flake-not-regular",
            Severity::Error,
            "Nix flake is not a regular file",
            "Refusing to inspect an indirect or non-regular flake.",
            Some(&flake),
            Vec::new(),
        ));
    } else if !lock.is_file() {
        diagnostics.push(diagnostic(
            "interop.nix.lock-missing",
            Severity::Warning,
            "Nix flake lock is missing",
            "nix develop cannot be checked reproducibly without the adjacent flake.lock.",
            Some(&lock),
            vec![RecommendedAction {
                id: "lock-nix-flake".to_string(),
                title: "Create the Nix flake lock".to_string(),
                kind: ActionKind::ExternalCommand,
                argv: vec!["nix".to_string(), "flake".to_string(), "lock".to_string()],
                working_directory: path_text(flake.parent().unwrap_or(root)),
                mutates_project: true,
                requires_network: true,
                executes_package_code: false,
                requires_confirmation: true,
            }],
        ));
    } else if !is_regular_file(&lock) {
        diagnostics.push(diagnostic(
            "interop.nix.lock-not-regular",
            Severity::Error,
            "Nix flake lock is not a regular file",
            "Refusing to inspect an indirect or non-regular flake lock.",
            Some(&lock),
            Vec::new(),
        ));
    } else {
        verified = read_bounded_text(&lock)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|value| {
                value
                    .get("root")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                    && value
                        .get("nodes")
                        .and_then(serde_json::Value::as_object)
                        .is_some()
            });
        if !verified {
            diagnostics.push(diagnostic(
                "interop.nix.lock-invalid",
                Severity::Error,
                "Nix flake lock is invalid",
                "flake.lock must be valid JSON with root and nodes entries before nix develop is recommended.",
                Some(&lock),
                Vec::new(),
            ));
        }
    }

    InteropStatus {
        detected: true,
        declared: true,
        verified,
        source: Some(path_text(&flake)),
    }
}

fn inspect_updates(
    root: &Path,
    cfg: &Config,
    manifest: Option<&Manifest>,
    lock: Option<&Lockfile>,
    updates: &mut Vec<VersionRecommendation>,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    let Ok(registry) = registry_for(&cfg.registry) else {
        diagnostics.push(update_check_failed(root));
        return false;
    };
    let mut checks = vec![(
        "zed-pkg/zed-cli".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    )];
    if let (Some(manifest), Some(lock)) = (manifest, lock) {
        for dependency in manifest.dependencies.keys() {
            let Some((org, name)) = dependency.split_once('/') else {
                continue;
            };
            if let Some(package) = lock.find(org, name) {
                checks.push((dependency.clone(), package.version.clone()));
            }
        }
    }
    checks.sort();
    checks.dedup();

    let mut failures = 0_usize;
    for (package, current) in checks {
        let Some((org, name)) = package.split_once('/') else {
            continue;
        };
        let Ok(metadata) = registry.get_package(org, name) else {
            failures += 1;
            continue;
        };
        let latest = metadata.latest.or_else(|| {
            metadata
                .versions
                .iter()
                .filter_map(|value| Version::parse(value).ok())
                .max()
                .map(|value| value.to_string())
        });
        let Some(latest) = latest else {
            continue;
        };
        let Some(change) = version_change(&current, &latest) else {
            continue;
        };
        updates.push(VersionRecommendation {
            package: package.clone(),
            current: current.clone(),
            latest: latest.clone(),
            change,
        });
        let action = if package == "zed-pkg/zed-cli" {
            zed_action(
                root,
                "self-update",
                "Install the current Zed CLI release",
                &["zed", "self-update"],
                true,
                true,
                false,
            )
        } else {
            zed_action(
                root,
                "review-dependency-update",
                "Review the dependency update",
                &["zed", "add", &format!("{package}@^{latest}")],
                true,
                true,
                false,
            )
        };
        diagnostics.push(diagnostic(
            "update.available",
            Severity::Info,
            "A newer package version is available",
            &format!("{package} can move from {current} to {latest} ({change:?})."),
            None,
            vec![action],
        ));
    }
    if failures > 0 {
        diagnostics.push(diagnostic(
            "update.check-partial",
            Severity::Warning,
            "Some version checks could not be completed",
            "The registry did not return public package metadata for every checked dependency.",
            None,
            Vec::new(),
        ));
    }
    failures == 0
}

fn update_check_failed(root: &Path) -> Diagnostic {
    diagnostic(
        "update.check-failed",
        Severity::Warning,
        "Version checks could not be started",
        "The configured registry is not available through the safe package-metadata client.",
        None,
        vec![zed_action(
            root,
            "retry-update-check",
            "Retry the update check",
            &["zed", "inspect", "--json", "--network"],
            false,
            true,
            false,
        )],
    )
}

fn version_change(current: &str, latest: &str) -> Option<VersionChange> {
    let current = Version::parse(current.trim_start_matches('v')).ok()?;
    let latest = Version::parse(latest.trim_start_matches('v')).ok()?;
    if latest <= current || !latest.pre.is_empty() {
        return None;
    }
    Some(if latest.major != current.major {
        VersionChange::Major
    } else if latest.minor != current.minor {
        VersionChange::Minor
    } else {
        VersionChange::Patch
    })
}

fn summarize(diagnostics: &[Diagnostic]) -> InspectionSummary {
    let errors = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Warning)
        .count();
    let information = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Info)
        .count();
    InspectionSummary {
        health: if errors > 0 {
            Health::Error
        } else if warnings > 0 {
            Health::Warning
        } else {
            Health::Healthy
        },
        errors,
        warnings,
        information,
    }
}

fn absolute_workspace(requested: &Path) -> Result<PathBuf> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()?.join(requested)
    };
    Ok(fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

fn read_bounded_text(path: &Path) -> Result<String> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{} is not a regular file",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_INSPECTION_FILE_BYTES,
        "{} exceeds the {}-byte inspection limit",
        path.display(),
        MAX_INSPECTION_FILE_BYTES
    );
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn diagnostic(
    code: &str,
    severity: Severity,
    title: &str,
    detail: &str,
    path: Option<&Path>,
    actions: Vec<RecommendedAction>,
) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity,
        title: title.to_string(),
        detail: detail.to_string(),
        location: path.map(|path| DiagnosticLocation {
            path: path_text(path),
            line: None,
            column: None,
        }),
        actions,
    }
}

fn zed_action(
    root: &Path,
    id: &str,
    title: &str,
    argv: &[&str],
    mutates_project: bool,
    requires_network: bool,
    executes_package_code: bool,
) -> RecommendedAction {
    RecommendedAction {
        id: id.to_string(),
        title: title.to_string(),
        kind: ActionKind::ZedCommand,
        argv: argv.iter().map(|value| (*value).to_string()).collect(),
        working_directory: path_text(root),
        mutates_project,
        requires_network,
        executes_package_code,
        requires_confirmation: mutates_project || requires_network || executes_package_code,
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn location_path(diagnostic: &Diagnostic) -> &str {
    diagnostic
        .location
        .as_ref()
        .map(|location| location.path.as_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(home: &Path) -> Config {
        Config {
            registry: "file:///unavailable".to_string(),
            home: home.to_path_buf(),
            token: None,
            auth_url: "http://127.0.0.1:9".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        }
    }

    #[test]
    fn version_recommendations_classify_semver_changes() {
        assert_eq!(version_change("1.2.3", "2.0.0"), Some(VersionChange::Major));
        assert_eq!(version_change("1.2.3", "1.3.0"), Some(VersionChange::Minor));
        assert_eq!(version_change("1.2.3", "1.2.4"), Some(VersionChange::Patch));
        assert_eq!(version_change("1.2.3", "1.2.3"), None);
        assert_eq!(version_change("1.2.3", "1.2.4-beta.1"), None);
    }

    #[test]
    fn network_mode_uses_registry_metadata_for_cli_updates() {
        let project = tempfile::tempdir().unwrap();
        let registry = tempfile::tempdir().unwrap();
        let package_dir = registry.path().join("packages/zed-pkg/zed-cli");
        fs::create_dir_all(&package_dir).unwrap();
        fs::write(
            package_dir.join("package.json"),
            serde_json::json!({
                "org": "zed-pkg",
                "name": "zed-cli",
                "vcs": "git",
                "repo_url": "https://github.com/zed-pkg/zed-cli",
                "latest": "0.1.1",
                "versions": ["0.1.1"]
            })
            .to_string(),
        )
        .unwrap();
        let mut cfg = config(project.path());
        cfg.registry = format!("file://{}", registry.path().display());

        let report = inspect(project.path(), &cfg, true).unwrap();

        assert!(report.network.update_check_complete);
        assert_eq!(report.updates.len(), 1);
        assert_eq!(report.updates[0].package, "zed-pkg/zed-cli");
        assert_eq!(report.updates[0].change, VersionChange::Patch);
    }

    #[test]
    fn interrupted_transaction_is_reported_without_recovery() {
        let project = tempfile::tempdir().unwrap();
        let staging = project.path().join(crate::transaction::STAGING_DIR);
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("journal"), "keep me").unwrap();

        let report = inspect(project.path(), &config(project.path()), false).unwrap();

        assert!(staging.join("journal").is_file());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.code == "transaction.interrupted")
        );
    }

    #[test]
    fn git_submodule_flag_is_explicit() {
        let project = tempfile::tempdir().unwrap();
        let manifest = project.path().join(MANIFEST_FILE);
        fs::write(
            &manifest,
            "[package]\norg = \"acme\"\nname = \"app\"\nversion = \"1.0.0\"\n\n[package.repository]\nurl = \"https://example.com/acme/app\"\n\n[interop.git]\nconsume_gitmodules = true\n",
        )
        .unwrap();

        assert!(crate::git_submodules::manifest_consumes_gitmodules(project.path()).unwrap());
    }

    #[test]
    fn invalid_git_submodule_flag_is_a_diagnostic() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join(MANIFEST_FILE),
            "[package]\norg = \"acme\"\nname = \"app\"\nversion = \"1.0.0\"\n\n[package.repository]\nurl = \"https://example.com/acme/app\"\n\n[interop.git]\nconsume_gitmodules = \"yes\"\n",
        )
        .unwrap();

        let report = inspect(project.path(), &config(project.path()), false).unwrap();

        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| { item.code == "interop.git-submodules.declaration-invalid" })
        );
        assert!(!report.interop.git_submodules.verified);
    }

    #[test]
    fn informational_severity_serializes_as_info() {
        assert_eq!(serde_json::to_value(Severity::Info).unwrap(), "info");
    }

    #[test]
    fn json_contract_uses_argv_and_camel_case() {
        let project = tempfile::tempdir().unwrap();
        let report = inspect(project.path(), &config(project.path()), false).unwrap();
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["schemaVersion"], INSPECTION_SCHEMA_VERSION);
        assert!(json.get("workspaceRoot").is_some());
        assert!(json["diagnostics"][0]["actions"][0]["argv"].is_array());
        assert!(
            json["diagnostics"][0]["actions"][0]
                .get("requiresConfirmation")
                .is_some()
        );
    }
}
