//! Read-only, offline project inspection for IDE integrations.
//!
//! `zed inspect` is intentionally dispatched before the ordinary CLI startup
//! path so it never loads credentials, initializes registry clients, or runs
//! transaction recovery. The implementation only reads project and local-store
//! metadata and emits one JSON document on stdout.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::error::ErrorKind;
use clap::{Command, CommandFactory, Parser, ValueEnum};
use serde::Serialize;
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR, store_entry_rel};

pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum InspectFormat {
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "inspect",
    about = "Inspect local Zed package state without mutation or network access"
)]
struct InspectArgs {
    /// Output format. JSON is the stable IDE integration contract.
    #[arg(long, value_enum, default_value = "json")]
    format: InspectFormat,

    /// Existing absolute project root to inspect.
    #[arg(long, value_name = "ABSOLUTE_PATH")]
    root: PathBuf,
}

pub fn command() -> Command {
    InspectArgs::command()
}

/// Intercept `zed inspect` before normal CLI startup loads process configuration
/// or runs recovery. Returns `None` for every other command.
pub fn dispatch(args: &[OsString]) -> Option<Result<i32>> {
    let index = inspect_command_index(args)?;
    let parse_args = std::iter::once(OsString::from("inspect"))
        .chain(args.iter().skip(index + 1).cloned())
        .collect::<Vec<_>>();
    let parsed = match InspectArgs::try_parse_from(parse_args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let kind = error.kind();
            if let Err(print_error) = error.print() {
                return Some(Err(print_error.into()));
            }
            return Some(Ok(
                if matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                    0
                } else {
                    2
                },
            ));
        }
    };
    Some(run(parsed).map(|()| 0))
}

fn inspect_command_index(args: &[OsString]) -> Option<usize> {
    let mut index = 1;
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if token == "inspect" {
            return Some(index);
        }
        if global_option_takes_value(&token) {
            index += if token.contains('=') { 1 } else { 2 };
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return None;
    }
    None
}

fn global_option_takes_value(token: &str) -> bool {
    const OPTIONS: &[&str] = &[
        "--registry",
        "--home",
        "--token",
        "--auth-url",
        "--supabase-url",
        "--supabase-key",
        "--global-bin-dir",
    ];
    OPTIONS.iter().any(|option| {
        token == *option
            || token
                .strip_prefix(option)
                .is_some_and(|remainder| remainder.starts_with('='))
    })
}

fn run(args: InspectArgs) -> Result<()> {
    debug_assert_eq!(args.format, InspectFormat::Json);
    if !args.root.is_absolute() {
        bail!("--root must be an absolute path");
    }
    if !args.root.is_dir() {
        bail!(
            "--root must name an existing directory: {}",
            args.root.display()
        );
    }
    let root = fs::canonicalize(&args.root)
        .with_context(|| format!("canonicalizing project root {}", args.root.display()))?;
    let report = inspect_project(&root)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &report)?;
    writeln!(&mut output)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Health {
    Healthy,
    Warning,
    Error,
}

#[derive(Debug, Serialize)]
struct InspectReport {
    schema_version: &'static str,
    root: String,
    cli: CliIdentity,
    package: ProjectPackageState,
    workspace_members: Vec<String>,
    adapter_outputs: Vec<AdapterOutput>,
    locked_packages: Vec<LockedPackageState>,
    summary: InspectSummary,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct CliIdentity {
    implementation: &'static str,
    command: &'static str,
    offline: bool,
    mutates_project: bool,
    loads_credentials: bool,
}

#[derive(Debug, Serialize)]
struct ProjectPackageState {
    manifest: String,
    lockfile: String,
    materialization_dir: String,
    identity: Option<PackageIdentity>,
}

#[derive(Debug, Serialize)]
struct PackageIdentity {
    org: String,
    name: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct AdapterOutput {
    kind: &'static str,
    path: String,
    present: bool,
}

#[derive(Debug, Serialize)]
struct LockedPackageState {
    org: String,
    name: String,
    version: String,
    sha256: String,
    store_present: Option<bool>,
    materialized: bool,
}

#[derive(Debug, Serialize)]
struct InspectSummary {
    health: Health,
    errors: usize,
    warnings: usize,
    frozen_ready: bool,
    recovery_pending: bool,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    code: &'static str,
    severity: Severity,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    location: Location,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<RecommendedAction>,
}

#[derive(Debug, Serialize)]
struct Location {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<usize>,
}

#[derive(Debug, Serialize)]
struct RecommendedAction {
    id: &'static str,
    title: &'static str,
    kind: &'static str,
    argv: Vec<&'static str>,
    cwd: String,
    mutates_project: bool,
    requires_network: bool,
    executes_package_code: bool,
}

fn inspect_project(root: &Path) -> Result<InspectReport> {
    let manifest_path = root.join(MANIFEST_FILE);
    let lock_path = root.join(LOCKFILE_FILE);
    let mut diagnostics = Vec::new();

    let manifest = read_manifest(&manifest_path, root, &mut diagnostics);
    let lockfile = read_lockfile(&lock_path, root, &mut diagnostics);

    let materialization_dir = manifest
        .as_ref()
        .and_then(|manifest| manifest.install.dir.as_deref())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(MODULES_DIR));
    let materialization_path = root.join(&materialization_dir);

    let recovery_path = root.join(crate::transaction::STAGING_DIR);
    let recovery_pending = recovery_path.is_dir();
    if recovery_pending {
        diagnostics.push(Diagnostic {
            code: "RECOVERY_PENDING",
            severity: Severity::Warning,
            message: "An interrupted project transaction is present; inspection left it untouched."
                .to_string(),
            detail: None,
            location: location(&recovery_path),
            actions: vec![install_action(
                root,
                true,
                "recover-frozen",
                "Recover with the locked graph",
            )],
        });
    }

    let mut direct_missing = BTreeSet::new();
    if let (Some(manifest), Some(lockfile)) = (&manifest, &lockfile) {
        for dependency in manifest
            .dependencies
            .keys()
            .chain(manifest.build_dependencies.keys())
        {
            let Some((org, name)) = dependency.split_once('/') else {
                continue;
            };
            if lockfile.find(org, name).is_none() {
                direct_missing.insert(dependency.clone());
            }
        }
        for dependency in &direct_missing {
            diagnostics.push(Diagnostic {
                code: "LOCK_STALE",
                severity: Severity::Warning,
                message: format!("Direct dependency `{dependency}` is not pinned in the lockfile."),
                detail: None,
                location: location(&lock_path),
                actions: vec![install_action(
                    root,
                    false,
                    "refresh-lock",
                    "Refresh the lockfile",
                )],
            });
        }
    }

    if let Some(lockfile) = &lockfile {
        if !lockfile.packages.is_empty() && !materialization_path.is_dir() {
            diagnostics.push(Diagnostic {
                code: "MATERIALIZATION_MISSING",
                severity: Severity::Warning,
                message: "The lockfile has packages but the materialization directory is absent."
                    .to_string(),
                detail: None,
                location: location(&materialization_path),
                actions: vec![install_action(
                    root,
                    true,
                    "restore-frozen",
                    "Restore locked packages",
                )],
            });
        }
    }

    let store_root = local_store_root();
    let locked_packages = lockfile
        .as_ref()
        .map(|lockfile| {
            lockfile
                .packages
                .iter()
                .map(|package| {
                    let materialized = materialization_path
                        .join(&package.org)
                        .join(&package.name)
                        .exists();
                    if materialization_path.is_dir() && !materialized {
                        diagnostics.push(Diagnostic {
                            code: "PACKAGE_NOT_MATERIALIZED",
                            severity: Severity::Warning,
                            message: format!(
                                "Locked package `{}/{}` is not materialized in the project.",
                                package.org, package.name
                            ),
                            detail: None,
                            location: location(
                                &materialization_path.join(&package.org).join(&package.name),
                            ),
                            actions: vec![install_action(
                                root,
                                true,
                                "restore-frozen",
                                "Restore locked packages",
                            )],
                        });
                    }
                    LockedPackageState {
                        org: package.org.clone(),
                        name: package.name.clone(),
                        version: package.version.clone(),
                        sha256: package.sha256.clone(),
                        store_present: store_root
                            .as_ref()
                            .map(|home| home.join(store_entry_rel(&package.sha256)).is_dir()),
                        materialized,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Warning)
        .count();
    let health = if errors > 0 {
        Health::Error
    } else if warnings > 0 {
        Health::Warning
    } else {
        Health::Healthy
    };
    let frozen_ready =
        manifest.is_some() && lockfile.is_some() && direct_missing.is_empty() && !recovery_pending;

    let identity = manifest.as_ref().map(|manifest| PackageIdentity {
        org: manifest.package.org.clone(),
        name: manifest.package.name.clone(),
        version: manifest.package.version.clone(),
    });
    let workspace_members = manifest.as_ref().map(workspace_members).unwrap_or_default();

    Ok(InspectReport {
        schema_version: SCHEMA_VERSION,
        root: display(root),
        cli: CliIdentity {
            implementation: "zed-pkg",
            command: "inspect",
            offline: true,
            mutates_project: false,
            loads_credentials: false,
        },
        package: ProjectPackageState {
            manifest: display(&manifest_path),
            lockfile: display(&lock_path),
            materialization_dir: display(&materialization_path),
            identity,
        },
        workspace_members,
        adapter_outputs: adapter_outputs(root),
        locked_packages,
        summary: InspectSummary {
            health,
            errors,
            warnings,
            frozen_ready,
            recovery_pending,
        },
        diagnostics,
    })
}

fn read_manifest(path: &Path, root: &Path, diagnostics: &mut Vec<Diagnostic>) -> Option<Manifest> {
    match fs::read_to_string(path) {
        Ok(text) => match Manifest::parse(&text) {
            Ok(manifest) => Some(manifest),
            Err(_) => {
                diagnostics.push(Diagnostic {
                    code: "MANIFEST_INVALID",
                    severity: Severity::Error,
                    message: "The project manifest is invalid.".to_string(),
                    detail: Some(
                        "Source content and parser excerpts are intentionally omitted from IDE diagnostics."
                            .to_string(),
                    ),
                    location: location(path),
                    actions: Vec::new(),
                });
                None
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            diagnostics.push(Diagnostic {
                code: "MANIFEST_MISSING",
                severity: Severity::Error,
                message: format!("No {MANIFEST_FILE} exists at the selected project root."),
                detail: None,
                location: location(path),
                actions: vec![RecommendedAction {
                    id: "initialize-manifest",
                    title: "Initialize a Zed package manifest",
                    kind: "zed-command",
                    argv: vec!["zed", "init"],
                    cwd: display(root),
                    mutates_project: true,
                    requires_network: false,
                    executes_package_code: false,
                }],
            });
            None
        }
        Err(_) => {
            diagnostics.push(Diagnostic {
                code: "MANIFEST_UNREADABLE",
                severity: Severity::Error,
                message: "The project manifest could not be read.".to_string(),
                detail: None,
                location: location(path),
                actions: Vec::new(),
            });
            None
        }
    }
}

fn read_lockfile(path: &Path, root: &Path, diagnostics: &mut Vec<Diagnostic>) -> Option<Lockfile> {
    match fs::read_to_string(path) {
        Ok(text) => match Lockfile::parse(&text) {
            Ok(lockfile) => Some(lockfile),
            Err(_) => {
                diagnostics.push(Diagnostic {
                    code: "LOCK_INVALID",
                    severity: Severity::Error,
                    message: "The project lockfile is invalid or unsupported.".to_string(),
                    detail: Some(
                        "Source content and parser excerpts are intentionally omitted from IDE diagnostics."
                            .to_string(),
                    ),
                    location: location(path),
                    actions: vec![install_action(root, false, "refresh-lock", "Refresh the lockfile")],
                });
                None
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            diagnostics.push(Diagnostic {
                code: "LOCK_MISSING",
                severity: Severity::Warning,
                message: format!("No {LOCKFILE_FILE} exists at the selected project root."),
                detail: None,
                location: location(path),
                actions: vec![install_action(
                    root,
                    false,
                    "create-lock",
                    "Resolve and create the lockfile",
                )],
            });
            None
        }
        Err(_) => {
            diagnostics.push(Diagnostic {
                code: "LOCK_UNREADABLE",
                severity: Severity::Error,
                message: "The project lockfile could not be read.".to_string(),
                detail: None,
                location: location(path),
                actions: Vec::new(),
            });
            None
        }
    }
}

fn workspace_members(manifest: &Manifest) -> Vec<String> {
    let Some(workspace) = manifest.workspace.as_ref() else {
        return Vec::new();
    };
    serde_json::to_value(workspace)
        .ok()
        .and_then(|value| value.get("members").cloned())
        .and_then(|members| members.as_array().cloned())
        .map(|members| {
            members
                .iter()
                .filter_map(|member| member.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn adapter_outputs(root: &Path) -> Vec<AdapterOutput> {
    const OUTPUTS: &[(&str, &str)] = &[
        ("go-work", ".zed/go.work"),
        ("pythonpath", ".zed/pythonpath"),
        ("cargo-paths", ".zed/cargo-paths.toml"),
        ("pub-deps", ".zed/pub-deps.yaml"),
    ];
    OUTPUTS
        .iter()
        .map(|(kind, relative)| {
            let path = root.join(relative);
            AdapterOutput {
                kind,
                path: display(&path),
                present: path.is_file(),
            }
        })
        .collect()
}

fn local_store_root() -> Option<PathBuf> {
    std::env::var_os("ZED_PKG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            dirs::home_dir().map(|home| home.join(zed_interfaces::paths::ZED_HOME_DIR_NAME))
        })
}

fn install_action(
    root: &Path,
    frozen: bool,
    id: &'static str,
    title: &'static str,
) -> RecommendedAction {
    let argv = if frozen {
        vec!["zed", "install", "--frozen"]
    } else {
        vec!["zed", "install"]
    };
    RecommendedAction {
        id,
        title,
        kind: "zed-command",
        argv,
        cwd: display(root),
        mutates_project: true,
        requires_network: true,
        executes_package_code: true,
    }
}

fn location(path: &Path) -> Location {
    Location {
        path: display(path),
        line: None,
        column: None,
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[package]
org = "acme"
name = "demo"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/demo"
"#;

    #[test]
    fn healthy_local_project_is_frozen_ready_without_mutation() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join(MANIFEST_FILE), MANIFEST).unwrap();
        fs::write(project.path().join(LOCKFILE_FILE), "version = 1\n").unwrap();

        let report = inspect_project(project.path()).unwrap();
        assert_eq!(report.summary.health, Health::Healthy);
        assert!(report.summary.frozen_ready);
        assert!(report.diagnostics.is_empty());
        assert_eq!(report.package.identity.unwrap().org, "acme");
        assert!(!project.path().join(MODULES_DIR).exists());
    }

    #[test]
    fn malformed_manifest_is_structured_and_does_not_echo_source_secrets() {
        let project = tempfile::tempdir().unwrap();
        let secret = "ghp_fake_secret_that_must_not_escape";
        fs::write(
            project.path().join(MANIFEST_FILE),
            format!("[package\nsecret = \"{secret}\"\n"),
        )
        .unwrap();
        fs::write(project.path().join(LOCKFILE_FILE), "version = 1\n").unwrap();

        let report = inspect_project(project.path()).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(report.summary.health, Health::Error);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "MANIFEST_INVALID")
        );
        assert!(!json.contains(secret));
    }

    #[test]
    fn pending_transaction_is_reported_but_never_recovered() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join(MANIFEST_FILE), MANIFEST).unwrap();
        fs::write(project.path().join(LOCKFILE_FILE), "version = 1\n").unwrap();
        let staging = project.path().join(crate::transaction::STAGING_DIR);
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("sentinel"), "keep").unwrap();

        let report = inspect_project(project.path()).unwrap();
        assert!(report.summary.recovery_pending);
        assert!(!report.summary.frozen_ready);
        let recovery = report
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "RECOVERY_PENDING")
            .expect("recovery diagnostic")
            .actions
            .first()
            .expect("recovery action");
        assert!(recovery.mutates_project);
        assert!(recovery.requires_network);
        assert!(recovery.executes_package_code);
        assert_eq!(
            fs::read_to_string(staging.join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn missing_direct_lock_entry_is_reported_as_stale() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join(MANIFEST_FILE),
            format!("{MANIFEST}\n[dependencies]\n\"acme/dep\" = \"^1\"\n"),
        )
        .unwrap();
        fs::write(project.path().join(LOCKFILE_FILE), "version = 1\n").unwrap();

        let report = inspect_project(project.path()).unwrap();
        assert!(!report.summary.frozen_ready);
        assert!(report.diagnostics.iter().any(|d| d.code == "LOCK_STALE"));
    }

    #[test]
    fn command_detection_skips_global_options_without_loading_them() {
        let args = vec![
            OsString::from("zed"),
            OsString::from("--token"),
            OsString::from("ignored-secret"),
            OsString::from("inspect"),
            OsString::from("--root"),
            OsString::from("/tmp/example"),
        ];
        assert_eq!(inspect_command_index(&args), Some(3));
        assert_eq!(std::ffi::OsStr::new("inspect"), args[3].as_os_str());
    }
}
