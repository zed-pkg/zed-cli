//! Deterministic, credential-free project inspection for editor integrations.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};
use zed_interfaces::version::VersionScheme;

use crate::cli::InspectFormat;
use crate::git_submodules::{
    self, GitSubmoduleInventory, ManifestGitSubmodules, ManifestGitSubmodulesDeclaration,
};

pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    ZedCommand,
    OpenFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action {
    pub id: String,
    pub title: String,
    pub kind: ActionKind,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub mutates_project: bool,
    pub requires_network: bool,
    pub executes_package_code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub detail: String,
    pub location: Location,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lockfile: Option<PathBuf>,
    pub materialization_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitSubmoduleState {
    pub name: String,
    pub path: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub initialized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zed_package: Option<String>,
    pub workspace_adopted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitSubmodulesState {
    /// `null` means the manifest did not declare an ownership policy.
    pub manifest_declared: Option<bool>,
    /// Default used by install/overtake when CLI and environment omit an
    /// explicit true/false override.
    pub effective_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gitmodules: Option<PathBuf>,
    pub entries: Vec<GitSubmoduleState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InteropState {
    pub git_submodules: GitSubmodulesState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub health: String,
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InspectReport {
    pub schema_version: &'static str,
    pub root: PathBuf,
    pub package: PackageState,
    pub interop: InteropState,
    pub summary: Summary,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn print(requested: &Path, format: InspectFormat) -> Result<()> {
    let report = inspect(requested)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    match format {
        InspectFormat::Json => serde_json::to_writer_pretty(&mut output, &report)
            .context("serializing inspection report")?,
    }
    output.write_all(b"\n")?;
    Ok(())
}

pub fn inspect(requested: &Path) -> Result<InspectReport> {
    let requested = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()
            .context("reading current directory for inspection")?
            .join(requested)
    };
    let root = fs::canonicalize(&requested).unwrap_or(requested);
    let manifest_path = root.join(MANIFEST_FILE);
    let lockfile_path = root.join(LOCKFILE_FILE);
    let mut diagnostics = Vec::new();

    let root_is_directory = fs::metadata(&root).is_ok_and(|metadata| metadata.is_dir());
    if !root_is_directory {
        diagnostics.push(diagnostic(
            "ROOT_UNREADABLE",
            Severity::Error,
            "Inspection root is not a readable directory",
            format!(
                "{} could not be opened as a project directory",
                root.display()
            ),
            &root,
            None,
        ));
        return Ok(finish_report(
            root,
            None,
            None,
            None,
            None,
            Vec::new(),
            diagnostics,
        ));
    }

    let manifest_metadata = fs::symlink_metadata(&manifest_path).ok();
    let manifest_exists = manifest_metadata.is_some();
    let mut manifest = None;
    let mut interop_declaration = None;

    if let Some(metadata) = manifest_metadata {
        if !metadata.file_type().is_file() {
            diagnostics.push(diagnostic(
                "MANIFEST_INDIRECT",
                Severity::Error,
                "The Zed manifest is not a regular file",
                "Static inspection refuses a symlink, directory, or other indirect manifest.",
                &manifest_path,
                None,
            ));
        } else {
            match fs::read_to_string(&manifest_path) {
                Ok(text) => match Manifest::parse(&text) {
                    Ok(parsed) => manifest = Some(parsed),
                    Err(error) => diagnostics.push(diagnostic(
                        "MANIFEST_INVALID",
                        Severity::Error,
                        "The Zed manifest is invalid",
                        error.to_string(),
                        &manifest_path,
                        None,
                    )),
                },
                Err(error) => diagnostics.push(diagnostic(
                    "MANIFEST_UNREADABLE",
                    Severity::Error,
                    "The Zed manifest could not be read",
                    error.to_string(),
                    &manifest_path,
                    None,
                )),
            }
            match git_submodules::manifest_git_submodules(&root) {
                Ok(Some(declaration)) if declaration.root == root => {
                    interop_declaration = Some(declaration);
                }
                Ok(_) => {}
                Err(error) => diagnostics.push(diagnostic(
                    "GITMODULES_POLICY_INVALID",
                    Severity::Error,
                    "The manifest's Git-submodule policy is invalid",
                    error.to_string(),
                    &manifest_path,
                    None,
                )),
            }
        }
    }

    let gitmodules_path = root.join(".gitmodules");
    let gitmodules_exists = fs::symlink_metadata(&gitmodules_path).is_ok();
    let declared = interop_declaration
        .as_ref()
        .and_then(|declaration| match declaration.value {
            ManifestGitSubmodules::Enabled => Some(true),
            ManifestGitSubmodules::Disabled => Some(false),
            ManifestGitSubmodules::Undeclared => None,
        });

    if gitmodules_exists && manifest_exists && declared.is_none() {
        let mut item = diagnostic(
            "GITMODULES_UNDECLARED",
            Severity::Warning,
            "The manifest does not declare who consumes .gitmodules",
            "Add `[interop] git-submodules = true` when Zed should synchronize and validate Git submodules, or set it to false to document Git-only ownership.",
            &manifest_path,
            interop_declaration.as_ref().and_then(|value| value.line),
        );
        item.actions.push(open_file_action(
            "open-manifest-interop",
            "Declare Git-submodule ownership",
            &manifest_path,
        ));
        diagnostics.push(item);
    }
    if declared == Some(true) && !gitmodules_exists {
        diagnostics.push(diagnostic(
            "GITMODULES_MISSING",
            Severity::Warning,
            "The manifest enables Git-submodule interop but .gitmodules is missing",
            "Either restore the committed .gitmodules file or disable `[interop].git-submodules`.",
            &manifest_path,
            interop_declaration.as_ref().and_then(|value| value.line),
        ));
    }

    let inventory = if gitmodules_exists {
        match git_submodules::inspect_inventory(&root) {
            Ok(inventory) => inventory,
            Err(error) => {
                diagnostics.push(diagnostic(
                    "GITMODULES_INVALID",
                    Severity::Error,
                    ".gitmodules cannot be safely consumed",
                    error.to_string(),
                    &gitmodules_path,
                    None,
                ));
                None
            }
        }
    } else {
        None
    };

    if let Some(inventory) = &inventory {
        if inventory.declarations.is_empty() {
            diagnostics.push(diagnostic(
                "GITMODULES_EMPTY",
                Severity::Warning,
                ".gitmodules contains no usable submodule declarations",
                "Remove the empty file or add complete path and URL declarations.",
                &gitmodules_path,
                None,
            ));
        }
        if let Err(error) = git_submodules::inspect_gitmodules_provenance(&inventory.root) {
            diagnostics.push(diagnostic(
                "GITMODULES_UNCOMMITTED",
                if declared == Some(true) {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                ".gitmodules is not immutable at superproject HEAD",
                error.to_string(),
                &gitmodules_path,
                None,
            ));
        }
    }

    let materialization_relative = manifest
        .as_ref()
        .map(|value| value.modules_dir().to_string())
        .unwrap_or_else(|| MODULES_DIR.to_string());
    let submodules = inspect_submodules(
        &root,
        inventory.as_ref(),
        manifest.as_ref(),
        &materialization_relative,
        declared,
        &mut diagnostics,
    );

    Ok(finish_report(
        root,
        manifest_exists.then_some(manifest_path),
        lockfile_path.is_file().then_some(lockfile_path),
        Some(materialization_relative),
        interop_declaration,
        submodules,
        diagnostics,
    ))
}

fn inspect_submodules(
    root: &Path,
    inventory: Option<&GitSubmoduleInventory>,
    manifest: Option<&Manifest>,
    materialization_relative: &str,
    declared: Option<bool>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<GitSubmoduleState> {
    let Some(inventory) = inventory else {
        return Vec::new();
    };
    let workspace_members = manifest
        .and_then(|value| value.workspace.as_ref())
        .map(|workspace| {
            workspace
                .members
                .iter()
                .map(|member| normalize_relative(member))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut package_paths = BTreeMap::<String, String>::new();
    let mut upstream_paths = BTreeMap::<String, String>::new();
    let mut states = Vec::with_capacity(inventory.declarations.len());

    for module in &inventory.declarations {
        let module_path = root.join(&module.path);
        let module_manifest_path = module_path.join(MANIFEST_FILE);
        let workspace_adopted = workspace_members
            .iter()
            .any(|member| member == &normalize_relative(&module.path));
        let child_manifest = read_regular_manifest(&module_manifest_path, diagnostics);
        let zed_package = child_manifest.as_ref().map(Manifest::full_name);

        if paths_overlap(materialization_relative, &module.path) {
            diagnostics.push(diagnostic(
                "GIT_SUBMODULE_MATERIALIZATION_CONFLICT",
                Severity::Error,
                "Zed materialization overlaps a Git submodule path",
                format!(
                    "`{materialization_relative}` and submodule `{}` cannot safely own overlapping filesystem trees.",
                    module.path
                ),
                &module_manifest_path,
                None,
            ));
        }

        let normalized_upstream = normalize_repository_url(&module.url);
        if let Some(previous) = upstream_paths.insert(normalized_upstream, module.path.clone())
            && previous != module.path
        {
            diagnostics.push(diagnostic(
                "GIT_SUBMODULE_DUPLICATE_UPSTREAM",
                Severity::Error,
                "The same upstream repository is configured more than once",
                format!(
                    "Submodule paths `{previous}` and `{}` both use `{}`.",
                    module.path, module.url
                ),
                &root.join(".gitmodules"),
                None,
            ));
        }

        if let (Some(root_manifest), Some(child)) = (manifest, child_manifest.as_ref()) {
            let package = child.full_name();
            if let Some(previous) = package_paths.insert(package.clone(), module.path.clone())
                && previous != module.path
            {
                diagnostics.push(diagnostic(
                    "GIT_SUBMODULE_DUPLICATE_PACKAGE",
                    Severity::Error,
                    "A Zed package identity is provided by multiple submodules",
                    format!("`{package}` appears at `{previous}` and `{}`.", module.path),
                    &module_manifest_path,
                    None,
                ));
            }
            if let Some(requirement) = root_manifest.dependencies.get(&package) {
                if !workspace_adopted {
                    let mut item = diagnostic(
                        "GIT_SUBMODULE_DUPLICATE_AUTHORITY",
                        Severity::Error,
                        "A package is declared as both a registry dependency and an unadopted submodule",
                        format!(
                            "`{package}` is present at `{}` but that path is not a `[workspace].members` entry.",
                            module.path
                        ),
                        &module_manifest_path,
                        None,
                    );
                    item.actions.push(zed_action(
                        "overtake-git-submodules",
                        "Adopt compatible submodules",
                        &["zed", "overtake", "--git-submodules"],
                        true,
                        true,
                    ));
                    diagnostics.push(item);
                } else if !requirement_matches_child(requirement, child) {
                    diagnostics.push(diagnostic(
                        "GIT_SUBMODULE_VERSION_DRIFT",
                        Severity::Error,
                        "The adopted dependency requirement does not exactly match the submodule package",
                        format!(
                            "`{package}` requires `{requirement}` but the checked-out package declares `{}`.",
                            child.package.version
                        ),
                        &module_manifest_path,
                        None,
                    ));
                }
            } else if workspace_adopted {
                diagnostics.push(diagnostic(
                    "GIT_SUBMODULE_WORKSPACE_ORPHAN",
                    Severity::Warning,
                    "An adopted submodule workspace member is not a direct dependency",
                    format!(
                        "`{package}` is listed through `{}` but absent from `[dependencies]`.",
                        module.path
                    ),
                    &module_manifest_path,
                    None,
                ));
            }

            if let Err(error) =
                git_submodules::validate_repository_url_for_interop(&child.package.repository.url)
            {
                diagnostics.push(diagnostic(
                    "GIT_SUBMODULE_REPOSITORY_UNSAFE",
                    Severity::Error,
                    "A submodule package declares an unsafe repository URL",
                    error.to_string(),
                    &module_manifest_path,
                    None,
                ));
            } else if normalize_repository_url(&module.url)
                != normalize_repository_url(&child.package.repository.url)
            {
                diagnostics.push(diagnostic(
                    "GIT_SUBMODULE_REPOSITORY_MISMATCH",
                    Severity::Warning,
                    "Submodule transport and package provenance URLs differ",
                    format!(
                        ".gitmodules uses `{}`, while `{package}` declares `{}`.",
                        module.url, child.package.repository.url
                    ),
                    &module_manifest_path,
                    None,
                ));
            }
        }

        let (initialized, commit) = match git_submodules::inspect_checkout(root, &module.path) {
            Ok(commit) => (true, Some(commit)),
            Err(error) => {
                let mut item = diagnostic(
                    "GIT_SUBMODULE_CHECKOUT_DRIFT",
                    if declared == Some(true) {
                        Severity::Error
                    } else {
                        Severity::Warning
                    },
                    "A configured Git submodule is not reproducibly checked out",
                    error.to_string(),
                    &module_path,
                    None,
                );
                item.actions.push(zed_action(
                    "restore-git-submodules",
                    "Synchronize Git submodules",
                    &["zed", "install", "--git-submodules"],
                    true,
                    true,
                ));
                diagnostics.push(item);
                (false, None)
            }
        };

        states.push(GitSubmoduleState {
            name: module.name.clone(),
            path: module.path.clone(),
            url: module.url.clone(),
            branch: module.branch.clone(),
            initialized,
            commit,
            zed_package,
            workspace_adopted,
        });
    }
    states
}

fn read_regular_manifest(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Option<Manifest> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            diagnostics.push(diagnostic(
                "GIT_SUBMODULE_MANIFEST_UNREADABLE",
                Severity::Error,
                "A submodule package manifest could not be inspected",
                error.to_string(),
                path,
                None,
            ));
            return None;
        }
    };
    if !metadata.file_type().is_file() {
        diagnostics.push(diagnostic(
            "GIT_SUBMODULE_MANIFEST_INDIRECT",
            Severity::Error,
            "A submodule package manifest is not a regular file",
            "Takeover and static inspection refuse indirect package identity metadata.",
            path,
            None,
        ));
        return None;
    }
    match fs::read_to_string(path) {
        Ok(text) => match Manifest::parse(&text) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                diagnostics.push(diagnostic(
                    "GIT_SUBMODULE_MANIFEST_INVALID",
                    Severity::Error,
                    "A submodule package manifest is invalid",
                    error.to_string(),
                    path,
                    None,
                ));
                None
            }
        },
        Err(error) => {
            diagnostics.push(diagnostic(
                "GIT_SUBMODULE_MANIFEST_UNREADABLE",
                Severity::Error,
                "A submodule package manifest could not be read",
                error.to_string(),
                path,
                None,
            ));
            None
        }
    }
}

fn requirement_matches_child(requirement: &str, child: &Manifest) -> bool {
    match child.package.version_scheme {
        VersionScheme::Opaque => requirement == child.package.version,
        VersionScheme::Semver | VersionScheme::Calver => {
            requirement == format!("={}", child.package.version)
        }
    }
}

fn finish_report(
    root: PathBuf,
    manifest: Option<PathBuf>,
    lockfile: Option<PathBuf>,
    materialization_relative: Option<String>,
    interop_declaration: Option<ManifestGitSubmodulesDeclaration>,
    entries: Vec<GitSubmoduleState>,
    diagnostics: Vec<Diagnostic>,
) -> InspectReport {
    let materialization_dir = root.join(materialization_relative.as_deref().unwrap_or(MODULES_DIR));
    let manifest_declared = interop_declaration
        .as_ref()
        .and_then(|declaration| match declaration.value {
            ManifestGitSubmodules::Enabled => Some(true),
            ManifestGitSubmodules::Disabled => Some(false),
            ManifestGitSubmodules::Undeclared => None,
        });
    let errors = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|item| item.severity == Severity::Warning)
        .count();
    let health = if errors > 0 {
        "error"
    } else if warnings > 0 {
        "warning"
    } else {
        "ok"
    };
    let gitmodules = fs::symlink_metadata(root.join(".gitmodules"))
        .is_ok()
        .then(|| root.join(".gitmodules"));
    InspectReport {
        schema_version: SCHEMA_VERSION,
        root,
        package: PackageState {
            manifest,
            lockfile,
            materialization_dir,
        },
        interop: InteropState {
            git_submodules: GitSubmodulesState {
                manifest_declared,
                effective_default: manifest_declared == Some(true),
                gitmodules,
                entries,
            },
        },
        summary: Summary {
            health: health.to_string(),
            errors,
            warnings,
        },
        diagnostics,
    }
}

fn diagnostic(
    code: &str,
    severity: Severity,
    message: &str,
    detail: impl Into<String>,
    path: &Path,
    line: Option<usize>,
) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity,
        message: message.to_string(),
        detail: detail.into(),
        location: Location {
            path: path.to_path_buf(),
            line,
            column: line.map(|_| 1),
        },
        actions: Vec::new(),
    }
}

fn zed_action(
    id: &str,
    title: &str,
    argv: &[&str],
    mutates_project: bool,
    requires_network: bool,
) -> Action {
    Action {
        id: id.to_string(),
        title: title.to_string(),
        kind: ActionKind::ZedCommand,
        argv: argv.iter().map(|value| (*value).to_string()).collect(),
        path: None,
        mutates_project,
        requires_network,
        executes_package_code: false,
    }
}

fn open_file_action(id: &str, title: &str, path: &Path) -> Action {
    Action {
        id: id.to_string(),
        title: title.to_string(),
        kind: ActionKind::OpenFile,
        argv: Vec::new(),
        path: Some(path.to_path_buf()),
        mutates_project: false,
        requires_network: false,
        executes_package_code: false,
    }
}

fn normalize_relative(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_string()
}

fn paths_overlap(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn normalize_repository_url(value: &str) -> String {
    let mut value = value
        .trim()
        .strip_prefix("git+")
        .unwrap_or(value.trim())
        .trim_end_matches('/')
        .to_ascii_lowercase();
    if value.ends_with(".git") {
        value.truncate(value.len() - 4);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_overlap_is_segment_aware() {
        assert!(paths_overlap(".vendor/.zed", ".vendor"));
        assert!(paths_overlap("vendor/client", "vendor/client"));
        assert!(!paths_overlap("vendor/client", "vendor/client-old"));
    }

    #[test]
    fn repository_url_normalization_is_stable() {
        assert_eq!(
            normalize_repository_url("git+HTTPS://Example.com/Acme/Client.git/"),
            "https://example.com/acme/client"
        );
    }

    #[test]
    fn checked_in_schema_matches_the_report_major_version() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/inspect-v1.json")).unwrap();
        assert_eq!(schema["properties"]["schema_version"]["const"], "1.0");
        assert_eq!(SCHEMA_VERSION, "1.0");
    }
}
