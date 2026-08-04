//! Deterministic Devbox and Flox environment generation.
//!
//! The exporter consumes the shared [`EnvironmentPlan`] as the source of truth.
//! It never invokes Devbox, Flox, Nix, package resolvers, or activation hooks.
//! Manager-native lock generation and validation remain a separate canary step.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use zed_interfaces::environment::{
    ActivationPolicy, EnvironmentPlan, EnvironmentValidationMode, SystemPackageRequirement,
    ToolRequirement,
};

const GENERATOR_SCHEMA: u32 = 1;
const DEFAULT_PLAN_PATH: &str = ".zed/environment-plan.json";
const FROZEN_INSTALL_COMMAND: &str = "zed install --frozen";

const DEVBOX_PLATFORMS: &[&str] = &[
    "aarch64-darwin",
    "aarch64-linux",
    "armv7l-linux",
    "i686-linux",
    "x86_64-darwin",
    "x86_64-linux",
];

const FLOX_PLATFORMS: &[&str] = &[
    "aarch64-darwin",
    "aarch64-linux",
    "x86_64-darwin",
    "x86_64-linux",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportManager {
    Devbox,
    Flox,
}

impl ExportManager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Devbox => "devbox",
            Self::Flox => "flox",
        }
    }

    pub fn default_output_path(self) -> &'static str {
        match self {
            Self::Devbox => "devbox.json",
            Self::Flox => ".flox/env/manifest.toml",
        }
    }

    pub fn default_receipt_path(self) -> &'static str {
        match self {
            Self::Devbox => ".zed/environment-exports/devbox.json",
            Self::Flox => ".zed/environment-exports/flox.json",
        }
    }

    fn supported_platforms(self) -> &'static [&'static str] {
        match self {
            Self::Devbox => DEVBOX_PLATFORMS,
            Self::Flox => FLOX_PLATFORMS,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub manager: ExportManager,
    pub plan_path: String,
    pub output_path: String,
    pub receipt_path: String,
    pub environment_plan_sha256: String,
    pub output_sha256: String,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PackageKind {
    Tool,
    SystemPackage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ManagerPackage {
    kind: PackageKind,
    logical_name: String,
    package_ref: String,
    version: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    platforms: Vec<String>,
    provenance_sha256: String,
}

#[derive(Serialize)]
struct ExportReceipt<'a> {
    schema: &'static str,
    generator_schema: u32,
    manager: ExportManager,
    plan_path: &'a str,
    input_sha256: &'a str,
    environment_plan_sha256: &'a str,
    output_path: &'a str,
    output_sha256: &'a str,
    activation: &'static str,
    native_lock_required: bool,
    packages: &'a [ManagerPackage],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct DevboxPackage {
    version: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    platforms: Vec<String>,
}

#[derive(Serialize)]
struct DevboxShell {
    init_hook: Vec<&'static str>,
}

#[derive(Serialize)]
struct DevboxDocument {
    packages: BTreeMap<String, DevboxPackage>,
    shell: DevboxShell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FloxPackage {
    #[serde(rename = "pkg-path")]
    pkg_path: String,
    version: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    systems: Vec<String>,
}

#[derive(Serialize)]
struct FloxHook {
    #[serde(rename = "on-activate")]
    on_activate: &'static str,
}

#[derive(Serialize)]
struct FloxOptions {
    systems: Vec<String>,
}

#[derive(Serialize)]
struct FloxDocument {
    version: u32,
    install: BTreeMap<String, FloxPackage>,
    hook: FloxHook,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<FloxOptions>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileDisposition {
    Missing,
    Identical,
}

pub fn export_environment(
    project_root: &Path,
    manager: ExportManager,
    plan_path: Option<&Path>,
    output_path: Option<&Path>,
    receipt_path: Option<&Path>,
) -> Result<ExportResult> {
    let root = project_root
        .canonicalize()
        .with_context(|| format!("failed to resolve project root {}", project_root.display()))?;

    let requested_plan = plan_path.unwrap_or_else(|| Path::new(DEFAULT_PLAN_PATH));
    let (plan_absolute, plan_relative) = resolve_project_input(&root, requested_plan, "plan")?;
    let raw_plan = fs::read(&plan_absolute).with_context(|| {
        format!(
            "failed to read environment plan {}",
            plan_absolute.display()
        )
    })?;
    let plan: EnvironmentPlan = serde_json::from_slice(&raw_plan)
        .with_context(|| format!("failed to parse environment plan `{plan_relative}`"))?;
    plan.validate(EnvironmentValidationMode::FrozenPortable)
        .context("environment plan is not frozen-portable")?;
    if plan.activation != ActivationPolicy::FrozenInstall {
        bail!(
            "environment plan activation must be `frozen-install` before exporting to {}",
            manager.as_str()
        );
    }

    let plan = plan.normalized();
    let packages = collect_manager_packages(&plan)?;
    validate_manager_platforms(manager, &plan.platforms, &packages)?;

    let output = match manager {
        ExportManager::Devbox => generate_devbox(&packages)?,
        ExportManager::Flox => generate_flox(&plan, &packages)?,
    };

    let requested_output = output_path.unwrap_or_else(|| Path::new(manager.default_output_path()));
    let (output_absolute, output_relative) =
        resolve_project_output(&root, requested_output, "manager output")?;
    let requested_receipt =
        receipt_path.unwrap_or_else(|| Path::new(manager.default_receipt_path()));
    let (receipt_absolute, receipt_relative) =
        resolve_project_output(&root, requested_receipt, "export receipt")?;
    if output_absolute == receipt_absolute {
        bail!("manager output and export receipt must use different paths");
    }

    let input_sha256 = sha256_hex(&raw_plan);
    let canonical_plan = plan.canonical_json_bytes()?;
    let environment_plan_sha256 = sha256_hex(&canonical_plan);
    let output_sha256 = sha256_hex(&output);
    let receipt = ExportReceipt {
        schema: "zed.environment-export-receipt/v1",
        generator_schema: GENERATOR_SCHEMA,
        manager,
        plan_path: &plan_relative,
        input_sha256: &input_sha256,
        environment_plan_sha256: &environment_plan_sha256,
        output_path: &output_relative,
        output_sha256: &output_sha256,
        activation: FROZEN_INSTALL_COMMAND,
        native_lock_required: true,
        packages: &packages,
    };
    let mut receipt_bytes = serde_json::to_vec_pretty(&receipt)
        .context("failed to serialize environment export receipt")?;
    receipt_bytes.push(b'\n');

    let changed = write_pair_fail_closed(
        &root,
        &output_absolute,
        &output,
        &receipt_absolute,
        &receipt_bytes,
    )?;

    Ok(ExportResult {
        manager,
        plan_path: plan_relative,
        output_path: output_relative,
        receipt_path: receipt_relative,
        environment_plan_sha256,
        output_sha256,
        changed,
    })
}

pub fn generate_devbox(packages: &[ManagerPackage]) -> Result<Vec<u8>> {
    let mut output_packages = BTreeMap::new();
    for package in packages {
        let candidate = DevboxPackage {
            version: package.version.clone(),
            platforms: package.platforms.clone(),
        };
        if let Some(existing) =
            output_packages.insert(package.package_ref.clone(), candidate.clone())
            && existing != candidate
        {
            bail!(
                "Devbox package `{}` is requested with conflicting versions or platforms",
                package.package_ref
            );
        }
    }

    let document = DevboxDocument {
        packages: output_packages,
        shell: DevboxShell {
            init_hook: vec![FROZEN_INSTALL_COMMAND],
        },
    };
    let mut bytes = serde_json::to_vec_pretty(&document)
        .context("failed to serialize deterministic Devbox configuration")?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn generate_flox(plan: &EnvironmentPlan, packages: &[ManagerPackage]) -> Result<Vec<u8>> {
    let mut install = BTreeMap::new();
    for package in packages {
        validate_flox_alias(&package.logical_name)?;
        let candidate = FloxPackage {
            pkg_path: package.package_ref.clone(),
            version: package.version.clone(),
            systems: package.platforms.clone(),
        };
        if install
            .insert(package.logical_name.clone(), candidate)
            .is_some()
        {
            bail!(
                "Flox install alias `{}` is declared more than once",
                package.logical_name
            );
        }
    }

    let document = FloxDocument {
        version: 1,
        install,
        hook: FloxHook {
            on_activate: FROZEN_INSTALL_COMMAND,
        },
        options: (!plan.platforms.is_empty()).then(|| FloxOptions {
            systems: plan.platforms.clone(),
        }),
    };
    let mut text = toml::to_string_pretty(&document)
        .context("failed to serialize deterministic Flox manifest")?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text.into_bytes())
}

fn collect_manager_packages(plan: &EnvironmentPlan) -> Result<Vec<ManagerPackage>> {
    let mut packages = Vec::new();
    let mut aliases = BTreeSet::new();

    for (name, requirement) in &plan.tools {
        if !aliases.insert(name.clone()) {
            bail!("duplicate manager package alias `{name}`");
        }
        packages.push(tool_package(plan, name, requirement)?);
    }
    for (name, requirement) in &plan.system_packages {
        if !aliases.insert(name.clone()) {
            bail!(
                "tool and system package both use manager alias `{name}`; rename one before export"
            );
        }
        packages.push(system_package(plan, name, requirement)?);
    }
    packages.sort_by(|left, right| {
        (&left.logical_name, &left.package_ref, &left.version).cmp(&(
            &right.logical_name,
            &right.package_ref,
            &right.version,
        ))
    });
    Ok(packages)
}

fn tool_package(
    plan: &EnvironmentPlan,
    name: &str,
    requirement: &ToolRequirement,
) -> Result<ManagerPackage> {
    require_nixpkgs_provider(name, requirement.provider.as_deref(), "tool")?;
    let package_ref = requirement
        .backend
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!(
            "tool `{name}` needs an exact nixpkgs package reference in `backend` for Devbox/Flox export"
        ))?;
    validate_package_ref(name, package_ref)?;
    let version = requirement
        .resolved
        .as_deref()
        .expect("frozen-portable EnvironmentPlan validation guarantees a resolved tool identity");
    Ok(ManagerPackage {
        kind: PackageKind::Tool,
        logical_name: name.to_string(),
        package_ref: package_ref.to_string(),
        version: version.to_string(),
        platforms: effective_platforms(
            &plan.platforms,
            &requirement.platforms,
            &format!("tool `{name}`"),
        )?,
        provenance_sha256: canonical_sha256(requirement)?,
    })
}

fn system_package(
    plan: &EnvironmentPlan,
    name: &str,
    requirement: &SystemPackageRequirement,
) -> Result<ManagerPackage> {
    require_nixpkgs_provider(name, requirement.provider.as_deref(), "system package")?;
    let package_ref = requirement
        .package_ref
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!(
            "system package `{name}` needs an exact nixpkgs `package-ref` for Devbox/Flox export"
        ))?;
    validate_package_ref(name, package_ref)?;
    let version = requirement.resolved.as_deref().expect(
        "frozen-portable EnvironmentPlan validation guarantees a resolved system package identity",
    );
    Ok(ManagerPackage {
        kind: PackageKind::SystemPackage,
        logical_name: name.to_string(),
        package_ref: package_ref.to_string(),
        version: version.to_string(),
        platforms: effective_platforms(
            &plan.platforms,
            &requirement.platforms,
            &format!("system package `{name}`"),
        )?,
        provenance_sha256: canonical_sha256(requirement)?,
    })
}

fn require_nixpkgs_provider(name: &str, provider: Option<&str>, kind: &str) -> Result<()> {
    match provider.map(str::trim) {
        Some("nixpkgs") => Ok(()),
        Some(value) => bail!(
            "{kind} `{name}` uses provider `{value}`; the initial Devbox/Flox exporter supports only explicit `nixpkgs` mappings"
        ),
        None => bail!(
            "{kind} `{name}` has no provider; set it to `nixpkgs` and supply an exact manager package reference"
        ),
    }
}

fn validate_package_ref(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.starts_with('.')
        || value.contains('@')
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '+')
        })
    {
        bail!(
            "package mapping for `{name}` has unsupported nixpkgs reference `{value}`; use a stable attribute path without versions, whitespace, paths, or URLs"
        );
    }
    Ok(())
}

fn validate_flox_alias(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('.')
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        bail!(
            "Flox alias `{value}` is unsupported by exporter schema v{GENERATOR_SCHEMA}; use ASCII letters, digits, dot, underscore, or hyphen"
        );
    }
    Ok(())
}

fn effective_platforms(
    plan_platforms: &[String],
    package_platforms: &[String],
    label: &str,
) -> Result<Vec<String>> {
    if !plan_platforms.is_empty() && !package_platforms.is_empty() {
        let plan = plan_platforms.iter().collect::<BTreeSet<_>>();
        let outside = package_platforms
            .iter()
            .filter(|platform| !plan.contains(platform))
            .cloned()
            .collect::<Vec<_>>();
        if !outside.is_empty() {
            bail!(
                "{label} declares platform(s) outside the plan-level platform set: {}",
                outside.join(", ")
            );
        }
    }
    Ok(if package_platforms.is_empty() {
        plan_platforms.to_vec()
    } else {
        package_platforms.to_vec()
    })
}

fn validate_manager_platforms(
    manager: ExportManager,
    plan_platforms: &[String],
    packages: &[ManagerPackage],
) -> Result<()> {
    let supported = manager
        .supported_platforms()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    for (field, platforms) in std::iter::once(("plan", plan_platforms)).chain(
        packages
            .iter()
            .map(|package| (package.logical_name.as_str(), package.platforms.as_slice())),
    ) {
        for platform in platforms {
            if !supported.contains(platform.as_str()) {
                bail!(
                    "{} export cannot represent platform `{platform}` from `{field}` in schema v{GENERATOR_SCHEMA}",
                    manager.as_str()
                );
            }
        }
    }
    Ok(())
}

fn canonical_sha256<T: Serialize>(value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value).context("failed to serialize package provenance")?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn resolve_project_input(root: &Path, requested: &Path, kind: &str) -> Result<(PathBuf, String)> {
    let relative = normalized_relative_path(requested, kind)?;
    let joined = root.join(path_from_slash(&relative));
    if !joined.is_file() {
        bail!("{kind} does not exist or is not a file: {relative}");
    }
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("failed to resolve {kind} `{relative}`"))?;
    if !canonical.starts_with(root) {
        bail!("{kind} `{relative}` resolves outside the project root");
    }
    Ok((canonical, relative))
}

fn resolve_project_output(root: &Path, requested: &Path, kind: &str) -> Result<(PathBuf, String)> {
    let relative = normalized_relative_path(requested, kind)?;
    let joined = root.join(path_from_slash(&relative));
    if let Ok(metadata) = fs::symlink_metadata(&joined)
        && metadata.file_type().is_symlink()
    {
        bail!("{kind} `{relative}` must not be a symlink");
    }
    let parent = joined
        .parent()
        .context("project output unexpectedly has no parent")?;
    let existing_ancestor = nearest_existing_ancestor(parent)?;
    let canonical_ancestor = existing_ancestor.canonicalize().with_context(|| {
        format!(
            "failed to resolve existing output ancestor {}",
            existing_ancestor.display()
        )
    })?;
    if !canonical_ancestor.starts_with(root) {
        bail!("{kind} `{relative}` traverses outside the project root");
    }
    Ok((joined, relative))
}

fn normalized_relative_path(path: &Path, kind: &str) -> Result<String> {
    if path.is_absolute() {
        bail!("{kind} path must be project-relative: {}", path.display());
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .with_context(|| format!("{kind} path must be valid UTF-8"))?
                    .to_string(),
            ),
            _ => bail!(
                "{kind} path must be normalized and project-relative: {}",
                path.display()
            ),
        }
    }
    if parts.is_empty() {
        bail!("{kind} path must name a project file");
    }
    Ok(parts.join("/"))
}

fn path_from_slash(value: &str) -> PathBuf {
    value.split('/').collect()
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if !current.pop() {
                    bail!("could not find an existing ancestor for {}", path.display());
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect output ancestor {}", current.display())
                });
            }
        }
    }
}

fn write_pair_fail_closed(
    root: &Path,
    output_path: &Path,
    output: &[u8],
    receipt_path: &Path,
    receipt: &[u8],
) -> Result<bool> {
    let output_state = preflight_file(output_path, output, "manager output")?;
    let receipt_state = preflight_file(receipt_path, receipt, "export receipt")?;
    if output_state == FileDisposition::Identical && receipt_state == FileDisposition::Identical {
        return Ok(false);
    }

    let mut wrote_output = false;
    if output_state == FileDisposition::Missing {
        persist_new_file(root, output_path, output, "manager output")?;
        wrote_output = true;
    }
    if receipt_state == FileDisposition::Missing
        && let Err(error) = persist_new_file(root, receipt_path, receipt, "export receipt")
    {
        if wrote_output {
            let _ = fs::remove_file(output_path);
        }
        return Err(error);
    }
    Ok(true)
}

fn preflight_file(path: &Path, expected: &[u8], kind: &str) -> Result<FileDisposition> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("{kind} path is not a regular file: {}", path.display());
            }
            let existing = fs::read(path)
                .with_context(|| format!("failed to read existing {kind} {}", path.display()))?;
            if existing == expected {
                Ok(FileDisposition::Identical)
            } else {
                bail!(
                    "refusing to overwrite conflicting existing {kind}: {}",
                    path.display()
                )
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(FileDisposition::Missing),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect {kind} path {}", path.display()))
        }
    }
}

fn persist_new_file(root: &Path, path: &Path, bytes: &[u8], kind: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("project output unexpectedly has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("failed to resolve directory {}", parent.display()))?;
    if !canonical_parent.starts_with(root) {
        bail!("{kind} parent traverses outside the project root");
    }

    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary {kind}"))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("failed to write temporary {kind}"))?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("failed to sync temporary {kind}"))?;
    temporary.persist_noclobber(path).map_err(|error| {
        anyhow::anyhow!(
            "failed to atomically persist {kind} {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::environment::{
        Checksum, ChecksumAlgorithm, SystemPackageRequirement, ToolRequirement,
    };

    fn checksum(digit: char) -> Checksum {
        Checksum {
            algorithm: ChecksumAlgorithm::Sha256,
            value: digit.to_string().repeat(64),
        }
    }

    fn fixture_plan() -> EnvironmentPlan {
        let mut plan = EnvironmentPlan {
            activation: ActivationPolicy::FrozenInstall,
            platforms: vec!["aarch64-darwin".to_string(), "x86_64-linux".to_string()],
            ..EnvironmentPlan::default()
        };
        plan.tools.insert(
            "node".to_string(),
            ToolRequirement {
                requirement: "^22".to_string(),
                resolved: Some("22.11.0".to_string()),
                provider: Some("nixpkgs".to_string()),
                backend: Some("nodejs_22".to_string()),
                source: None,
                checksums: vec![checksum('a')],
                platforms: Vec::new(),
            },
        );
        plan.system_packages.insert(
            "git".to_string(),
            SystemPackageRequirement {
                requirement: "2.47.0".to_string(),
                resolved: Some("2.47.0".to_string()),
                provider: Some("nixpkgs".to_string()),
                package_ref: Some("gitFull".to_string()),
                source: None,
                checksums: vec![checksum('b')],
                platforms: vec!["x86_64-linux".to_string()],
            },
        );
        plan
    }

    fn packages(plan: &EnvironmentPlan) -> Vec<ManagerPackage> {
        let plan = plan.normalized();
        plan.validate(EnvironmentValidationMode::FrozenPortable)
            .unwrap();
        collect_manager_packages(&plan).unwrap()
    }

    #[test]
    fn devbox_generation_is_deterministic_and_keeps_platforms_separate() {
        let plan = fixture_plan();
        let first = generate_devbox(&packages(&plan)).unwrap();
        let second = generate_devbox(&packages(&plan)).unwrap();
        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(value["packages"]["nodejs_22"]["version"], "22.11.0");
        assert_eq!(
            value["packages"]["nodejs_22"]["platforms"],
            serde_json::json!(["aarch64-darwin", "x86_64-linux"])
        );
        assert_eq!(
            value["shell"]["init_hook"],
            serde_json::json!([FROZEN_INSTALL_COMMAND])
        );
        assert!(
            !value["packages"]["nodejs_22"]["version"]
                .as_str()
                .unwrap()
                .contains("linux")
        );
    }

    #[test]
    fn flox_generation_is_deterministic_and_parseable() {
        let plan = fixture_plan().normalized();
        let first = generate_flox(&plan, &packages(&plan)).unwrap();
        let second = generate_flox(&plan, &packages(&plan)).unwrap();
        assert_eq!(first, second);
        let value: toml::Value = toml::from_str(std::str::from_utf8(&first).unwrap()).unwrap();
        assert_eq!(value["version"].as_integer(), Some(1));
        assert_eq!(
            value["install"]["node"]["pkg-path"].as_str(),
            Some("nodejs_22")
        );
        assert_eq!(
            value["install"]["node"]["version"].as_str(),
            Some("22.11.0")
        );
        assert_eq!(
            value["hook"]["on-activate"].as_str(),
            Some(FROZEN_INSTALL_COMMAND)
        );
    }

    #[test]
    fn moving_versions_fail_before_generation() {
        let mut plan = fixture_plan();
        plan.tools.get_mut("node").unwrap().resolved = Some("latest".to_string());
        let error = plan
            .validate(EnvironmentValidationMode::FrozenPortable)
            .unwrap_err();
        assert!(error.to_string().contains("moving selector"));
    }

    #[test]
    fn provider_mapping_is_explicit_and_fail_closed() {
        let mut plan = fixture_plan();
        plan.tools.get_mut("node").unwrap().provider = Some("core".to_string());
        let error = collect_manager_packages(&plan.normalized()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("supports only explicit `nixpkgs`")
        );
    }

    #[test]
    fn package_platforms_must_be_inside_plan_platforms() {
        let mut plan = fixture_plan();
        plan.tools.get_mut("node").unwrap().platforms = vec!["aarch64-linux".to_string()];
        let error = collect_manager_packages(&plan.normalized()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the plan-level platform set")
        );
    }

    #[test]
    fn conflicting_devbox_package_identity_is_rejected() {
        let package = ManagerPackage {
            kind: PackageKind::Tool,
            logical_name: "node".to_string(),
            package_ref: "nodejs_22".to_string(),
            version: "22.11.0".to_string(),
            platforms: vec!["x86_64-linux".to_string()],
            provenance_sha256: "a".repeat(64),
        };
        let mut conflicting = package.clone();
        conflicting.logical_name = "node-secondary".to_string();
        conflicting.version = "22.12.0".to_string();
        let error = generate_devbox(&[package, conflicting]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicting versions or platforms")
        );
    }

    #[test]
    fn export_is_idempotent_and_refuses_human_owned_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".zed")).unwrap();
        let plan = fixture_plan();
        fs::write(
            temp.path().join(DEFAULT_PLAN_PATH),
            serde_json::to_vec_pretty(&plan).unwrap(),
        )
        .unwrap();

        let first =
            export_environment(temp.path(), ExportManager::Devbox, None, None, None).unwrap();
        assert!(first.changed);
        let output = fs::read(temp.path().join("devbox.json")).unwrap();
        let receipt = fs::read(temp.path().join(".zed/environment-exports/devbox.json")).unwrap();

        let second =
            export_environment(temp.path(), ExportManager::Devbox, None, None, None).unwrap();
        assert!(!second.changed);
        assert_eq!(fs::read(temp.path().join("devbox.json")).unwrap(), output);
        assert_eq!(
            fs::read(temp.path().join(".zed/environment-exports/devbox.json")).unwrap(),
            receipt
        );

        fs::write(temp.path().join("devbox.json"), b"{\"human\":true}\n").unwrap();
        let error =
            export_environment(temp.path(), ExportManager::Devbox, None, None, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing to overwrite conflicting")
        );
    }

    #[test]
    fn output_cannot_escape_project_root() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".zed")).unwrap();
        fs::write(
            temp.path().join(DEFAULT_PLAN_PATH),
            serde_json::to_vec_pretty(&fixture_plan()).unwrap(),
        )
        .unwrap();

        let error = export_environment(
            temp.path(),
            ExportManager::Flox,
            None,
            Some(Path::new("../manifest.toml")),
            None,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("normalized and project-relative")
        );
    }
}
