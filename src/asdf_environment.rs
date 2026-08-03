//! Project-local asdf interoperability.
//!
//! This adapter reads one `.tool-versions` file and an optional Zed-owned
//! `.zed/asdf.lock.toml` provenance sidecar. It never invokes `asdf`, searches
//! parent directories, reads user-global configuration, installs plugins, or
//! executes plugin code.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zed_interfaces::environment::{
    ActivationPolicy, Checksum, ChecksumAlgorithm, EnvironmentManager, EnvironmentPlan,
    EnvironmentSource, EnvironmentValidationMode, ImmutableSource, ToolRequirement,
};

const DEFAULT_CONFIG_PATH: &str = ".tool-versions";
const DEFAULT_LOCK_PATH: &str = ".zed/asdf.lock.toml";
const ASDF_LOCK_SCHEMA: u32 = 1;

#[derive(Debug, Clone)]
pub struct ImportedAsdfEnvironment {
    pub plan: EnvironmentPlan,
    pub config_path: String,
    pub lock_path: Option<String>,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
struct ConfiguredTool {
    requirement: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AsdfLockDocument {
    schema: u32,
    #[serde(default)]
    plugins: BTreeMap<String, AsdfPluginLock>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AsdfPluginLock {
    version: String,
    url: String,
    revision: String,
    sha256: String,
    #[serde(default)]
    platforms: Vec<String>,
}

#[derive(Serialize)]
struct NormalizedAsdfInputs<'a> {
    schema: u32,
    tools: &'a BTreeMap<String, ConfiguredTool>,
    lock: Option<&'a AsdfLockDocument>,
}

pub fn import_asdf(
    cwd: &Path,
    config: Option<&Path>,
    lock: Option<&Path>,
    frozen: bool,
) -> Result<ImportedAsdfEnvironment> {
    let root = cwd
        .canonicalize()
        .with_context(|| format!("failed to resolve project root {}", cwd.display()))?;

    let config_path = match config {
        Some(path) => resolve_project_file(&root, path, "asdf config")?,
        None => resolve_project_file(
            &root,
            Path::new(DEFAULT_CONFIG_PATH),
            "project-local asdf config",
        )?,
    };
    let config_relative = project_relative_path(&root, &config_path)?;
    let config_text = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let configured = parse_tool_versions(&config_text, &config_relative)?;

    let lock_path = resolve_lock_path(&root, lock)?;
    if frozen && lock_path.is_none() {
        bail!(
            "frozen asdf import requires `{DEFAULT_LOCK_PATH}` (or --lock) with exact plugin and artifact provenance"
        );
    }

    let (locked, lock_relative) = match lock_path {
        Some(path) => {
            let relative = project_relative_path(&root, &path)?;
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let parsed = parse_lock_document(&text, &relative)?;
            verify_lock_coverage(&configured, &parsed, &relative)?;
            (Some(parsed), Some(relative))
        }
        None => (None, None),
    };

    let source_digest = digest_manager_inputs(&configured, locked.as_ref())?;
    let mut plan = EnvironmentPlan {
        activation: ActivationPolicy::FrozenInstall,
        ..EnvironmentPlan::default()
    };

    for (name, configured_tool) in &configured {
        let plugin = locked
            .as_ref()
            .and_then(|document| document.plugins.get(name));
        let (resolved, source, checksums, platforms) = match plugin {
            Some(plugin) => (
                Some(plugin.version.clone()),
                Some(ImmutableSource {
                    url: plugin.url.clone(),
                    revision: plugin.revision.clone(),
                    subdir: None,
                    checksums: Vec::new(),
                }),
                vec![Checksum {
                    algorithm: ChecksumAlgorithm::Sha256,
                    value: plugin.sha256.clone(),
                }],
                plugin.platforms.clone(),
            ),
            None => (None, None, Vec::new(), Vec::new()),
        };

        plan.tools.insert(
            name.clone(),
            ToolRequirement {
                requirement: configured_tool.requirement.clone(),
                resolved,
                provider: Some("asdf".to_string()),
                backend: Some(format!("asdf-plugin:{name}")),
                source,
                checksums,
                platforms,
            },
        );
    }

    plan.sources.push(EnvironmentSource {
        manager: EnvironmentManager::Asdf,
        path: config_relative.clone(),
        lock_path: lock_relative.clone(),
        digest: Some(source_digest),
    });

    let validation_mode = if frozen {
        EnvironmentValidationMode::FrozenPortable
    } else {
        EnvironmentValidationMode::Authoring
    };
    plan.validate(validation_mode)
        .context("asdf environment did not satisfy the shared environment contract")?;

    let canonical = plan.canonical_json_bytes()?;
    let digest = hex::encode(Sha256::digest(&canonical));

    Ok(ImportedAsdfEnvironment {
        plan: plan.normalized(),
        config_path: config_relative,
        lock_path: lock_relative,
        digest,
    })
}

pub fn print_import(imported: &ImportedAsdfEnvironment, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&imported.plan)?);
        return Ok(());
    }

    println!("manager: asdf");
    println!("config: {}", imported.config_path);
    println!(
        "lock: {}",
        imported
            .lock_path
            .as_deref()
            .unwrap_or("<none; authoring mode>")
    );
    println!("tools: {}", imported.plan.tools.len());
    println!("environment-plan-sha256: {}", imported.digest);
    Ok(())
}

pub fn print_verification(imported: &ImportedAsdfEnvironment, json: bool) -> Result<()> {
    if json {
        let output = serde_json::json!({
            "manager": "asdf",
            "config": imported.config_path,
            "lock": imported.lock_path,
            "tools": imported.plan.tools.len(),
            "environment_plan_sha256": imported.digest,
            "verified": true,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!(
        "verified asdf environment: {} tool(s), digest {}",
        imported.plan.tools.len(),
        imported.digest
    );
    Ok(())
}

fn resolve_lock_path(root: &Path, requested: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = requested {
        return resolve_project_file(root, path, "asdf provenance lock").map(Some);
    }

    let candidate = root.join(DEFAULT_LOCK_PATH);
    if candidate.is_file() {
        resolve_project_file(root, &candidate, "asdf provenance lock").map(Some)
    } else {
        Ok(None)
    }
}

fn resolve_project_file(root: &Path, requested: &Path, kind: &str) -> Result<PathBuf> {
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    if !candidate.is_file() {
        bail!("{kind} does not exist or is not a file: {}", candidate.display());
    }
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", candidate.display()))?;
    if !canonical.starts_with(root) {
        bail!(
            "{kind} escapes the project root: {} resolves outside {}",
            candidate.display(),
            root.display()
        );
    }
    Ok(canonical)
}

fn project_relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .context("asdf paths must be valid UTF-8")?
                    .to_string(),
            ),
            _ => bail!("asdf paths must be normalized and project-relative"),
        }
    }
    if parts.is_empty() {
        bail!("asdf path must name a project file");
    }
    Ok(parts.join("/"))
}

fn parse_tool_versions(input: &str, path: &str) -> Result<BTreeMap<String, ConfiguredTool>> {
    let mut tools = BTreeMap::new();
    for (index, original) in input.lines().enumerate() {
        let line = original.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 {
            bail!(
                "`{path}` line {} must contain exactly one plugin and one version; multiple fallback versions cannot be represented losslessly by EnvironmentPlan v1",
                index + 1
            );
        }
        let name = fields[0];
        let requirement = fields[1];
        validate_tool_name(name, path, index + 1)?;
        if requirement.is_empty() {
            bail!("`{path}` line {} has an empty version", index + 1);
        }
        if tools
            .insert(
                name.to_string(),
                ConfiguredTool {
                    requirement: requirement.to_string(),
                },
            )
            .is_some()
        {
            bail!("duplicate asdf plugin `{name}` in `{path}`");
        }
    }
    if tools.is_empty() {
        bail!("`{path}` contains no asdf tool versions");
    }
    Ok(tools)
}

fn validate_tool_name(name: &str, path: &str, line: usize) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("invalid asdf plugin name `{name}` in `{path}` line {line}");
    }
    Ok(())
}

fn parse_lock_document(input: &str, path: &str) -> Result<AsdfLockDocument> {
    let document: AsdfLockDocument = toml::from_str(input)
        .with_context(|| format!("failed to parse asdf provenance lock `{path}`"))?;
    if document.schema != ASDF_LOCK_SCHEMA {
        bail!(
            "unsupported asdf provenance schema {} in `{path}`; this build supports {ASDF_LOCK_SCHEMA}",
            document.schema
        );
    }
    if document.plugins.is_empty() {
        bail!("asdf provenance lock `{path}` has no [plugins] entries");
    }

    let mut normalized = document;
    for (name, plugin) in &mut normalized.plugins {
        plugin.version = plugin.version.trim().to_string();
        plugin.url = plugin.url.trim().to_string();
        plugin.revision = plugin.revision.trim().to_ascii_lowercase();
        plugin.sha256 = plugin.sha256.trim().to_ascii_lowercase();
        plugin.platforms = plugin
            .platforms
            .iter()
            .map(|platform| platform.trim().to_string())
            .collect();
        plugin.platforms.sort();
        plugin.platforms.dedup();
        validate_plugin_lock(name, plugin, path)?;
    }
    Ok(normalized)
}

fn validate_plugin_lock(name: &str, plugin: &AsdfPluginLock, path: &str) -> Result<()> {
    if plugin.version.is_empty() {
        bail!("`plugins.{name}.version` in `{path}` must not be empty");
    }
    if plugin.url.is_empty() {
        bail!("`plugins.{name}.url` in `{path}` must not be empty");
    }
    if is_local_reference(&plugin.url) {
        bail!(
            "`plugins.{name}.url` in `{path}` is non-portable: {}",
            plugin.url
        );
    }
    if https_url_has_userinfo(&plugin.url) {
        bail!(
            "`plugins.{name}.url` in `{path}` contains user information; commit only credential-free plugin URLs"
        );
    }
    if !is_full_hex_revision(&plugin.revision) {
        bail!(
            "`plugins.{name}.revision` in `{path}` must be a full 40- or 64-character hexadecimal commit"
        );
    }
    if plugin.sha256.len() != 64
        || !plugin
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!(
            "`plugins.{name}.sha256` in `{path}` must be 64 hexadecimal characters"
        );
    }
    for platform in &plugin.platforms {
        if platform.is_empty()
            || platform
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            bail!(
                "`plugins.{name}.platforms` in `{path}` contains invalid platform `{platform}`"
            );
        }
    }
    Ok(())
}

fn verify_lock_coverage(
    configured: &BTreeMap<String, ConfiguredTool>,
    locked: &AsdfLockDocument,
    path: &str,
) -> Result<()> {
    let configured_names = configured.keys().cloned().collect::<BTreeSet<_>>();
    let locked_names = locked.plugins.keys().cloned().collect::<BTreeSet<_>>();
    let missing = configured_names
        .difference(&locked_names)
        .cloned()
        .collect::<Vec<_>>();
    let extra = locked_names
        .difference(&configured_names)
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() || !extra.is_empty() {
        bail!(
            "asdf config/lock drift in `{path}`: missing locked plugins [{}]; extra locked plugins [{}]",
            missing.join(", "),
            extra.join(", ")
        );
    }

    for (name, configured_tool) in configured {
        let plugin = locked
            .plugins
            .get(name)
            .expect("coverage was checked before version comparison");
        if plugin.version != configured_tool.requirement {
            bail!(
                "asdf version drift for `{name}` in `{path}`: .tool-versions selects `{}` but the provenance lock records `{}`",
                configured_tool.requirement,
                plugin.version
            );
        }
    }
    Ok(())
}

fn digest_manager_inputs(
    configured: &BTreeMap<String, ConfiguredTool>,
    locked: Option<&AsdfLockDocument>,
) -> Result<Checksum> {
    let normalized = NormalizedAsdfInputs {
        schema: ASDF_LOCK_SCHEMA,
        tools: configured,
        lock: locked,
    };
    let bytes = serde_json::to_vec(&normalized)
        .context("failed to serialize normalized asdf manager state")?;
    let mut hasher = Sha256::new();
    hasher.update(b"zed-pkg:asdf-input:v1\0");
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(Checksum {
        algorithm: ChecksumAlgorithm::Sha256,
        value: hex::encode(hasher.finalize()),
    })
}

fn is_local_reference(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("path:")
        || value.starts_with("file:")
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.as_bytes().get(1) == Some(&b':')
}

fn is_full_hex_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn https_url_has_userinfo(value: &str) -> bool {
    let Some(authority_and_path) = value.strip_prefix("https://") else {
        return false;
    };
    authority_and_path
        .split('/')
        .next()
        .is_some_and(|authority| authority.contains('@'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(digit: char) -> String {
        digit.to_string().repeat(40)
    }

    fn checksum(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    fn write_lock(root: &Path, node_checksum: char, python_checksum: char) {
        fs::create_dir_all(root.join(".zed")).unwrap();
        fs::write(
            root.join(DEFAULT_LOCK_PATH),
            format!(
                r#"schema = 1

[plugins.nodejs]
version = "22.11.0"
url = "https://github.com/asdf-vm/asdf-nodejs.git"
revision = "{}"
sha256 = "{}"
platforms = ["x86_64-linux", "aarch64-darwin"]

[plugins.python]
version = "3.12.4"
url = "https://github.com/danhper/asdf-python.git"
revision = "{}"
sha256 = "{}"
"#,
                revision('1'),
                checksum(node_checksum),
                revision('2'),
                checksum(python_checksum),
            ),
        )
        .unwrap();
    }

    #[test]
    fn imports_exact_project_local_asdf_provenance() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.11.0\npython 3.12.4\n",
        )
        .unwrap();
        write_lock(temp.path(), 'a', 'b');

        let imported = import_asdf(temp.path(), None, None, true).unwrap();
        assert_eq!(imported.config_path, DEFAULT_CONFIG_PATH);
        assert_eq!(imported.lock_path.as_deref(), Some(DEFAULT_LOCK_PATH));
        assert_eq!(
            imported.plan.tools["nodejs"].resolved.as_deref(),
            Some("22.11.0")
        );
        assert_eq!(
            imported.plan.tools["nodejs"]
                .source
                .as_ref()
                .unwrap()
                .revision,
            revision('1')
        );
        assert_eq!(imported.plan.tools["nodejs"].checksums.len(), 1);
        assert_eq!(imported.digest.len(), 64);
    }

    #[test]
    fn authoring_import_needs_no_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.11.0\n",
        )
        .unwrap();

        let imported = import_asdf(temp.path(), None, None, false).unwrap();
        assert!(imported.lock_path.is_none());
        assert!(imported.plan.tools["nodejs"].resolved.is_none());
    }

    #[test]
    fn frozen_import_requires_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.11.0\n",
        )
        .unwrap();

        let error = import_asdf(temp.path(), None, None, true).unwrap_err();
        assert!(error.to_string().contains("requires `.zed/asdf.lock.toml`"));
    }

    #[test]
    fn multiple_fallback_versions_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.11.0 20.18.0\n",
        )
        .unwrap();

        let error = import_asdf(temp.path(), None, None, false).unwrap_err();
        assert!(error.to_string().contains("exactly one plugin and one version"));
    }

    #[test]
    fn lock_coverage_and_versions_must_match() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.12.0\npython 3.12.4\n",
        )
        .unwrap();
        write_lock(temp.path(), 'a', 'b');

        let error = import_asdf(temp.path(), None, None, true).unwrap_err();
        assert!(error.to_string().contains("asdf version drift"));
    }

    #[test]
    fn mutable_plugin_revision_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.11.0\n",
        )
        .unwrap();
        fs::create_dir_all(temp.path().join(".zed")).unwrap();
        fs::write(
            temp.path().join(DEFAULT_LOCK_PATH),
            format!(
                r#"schema = 1

[plugins.nodejs]
version = "22.11.0"
url = "https://github.com/asdf-vm/asdf-nodejs.git"
revision = "main"
sha256 = "{}"
"#,
                checksum('a')
            ),
        )
        .unwrap();

        let error = import_asdf(temp.path(), None, None, true).unwrap_err();
        assert!(error.to_string().contains("full 40- or 64-character"));
    }

    #[test]
    fn semantic_digest_ignores_comments_and_order() {
        let first = tempfile::tempdir().unwrap();
        fs::write(
            first.path().join(DEFAULT_CONFIG_PATH),
            "# first\nnodejs 22.11.0\npython 3.12.4\n",
        )
        .unwrap();
        write_lock(first.path(), 'a', 'b');

        let second = tempfile::tempdir().unwrap();
        fs::write(
            second.path().join(DEFAULT_CONFIG_PATH),
            "python 3.12.4\nnodejs 22.11.0 # second\n",
        )
        .unwrap();
        fs::create_dir_all(second.path().join(".zed")).unwrap();
        fs::write(
            second.path().join(DEFAULT_LOCK_PATH),
            format!(
                r#"schema = 1

[plugins.python]
sha256 = "{}"
revision = "{}"
url = "https://github.com/danhper/asdf-python.git"
version = "3.12.4"

[plugins.nodejs]
platforms = ["aarch64-darwin", "x86_64-linux"]
sha256 = "{}"
revision = "{}"
url = "https://github.com/asdf-vm/asdf-nodejs.git"
version = "22.11.0"
"#,
                checksum('b'),
                revision('2'),
                checksum('a'),
                revision('1'),
            ),
        )
        .unwrap();

        let left = import_asdf(first.path(), None, None, true).unwrap();
        let right = import_asdf(second.path(), None, None, true).unwrap();
        assert_eq!(left.digest, right.digest);
        assert_eq!(
            left.plan.sources[0].digest.as_ref(),
            right.plan.sources[0].digest.as_ref()
        );
    }

    #[test]
    fn artifact_checksum_changes_environment_identity() {
        let first = tempfile::tempdir().unwrap();
        fs::write(
            first.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.11.0\npython 3.12.4\n",
        )
        .unwrap();
        write_lock(first.path(), 'a', 'b');

        let second = tempfile::tempdir().unwrap();
        fs::write(
            second.path().join(DEFAULT_CONFIG_PATH),
            "nodejs 22.11.0\npython 3.12.4\n",
        )
        .unwrap();
        write_lock(second.path(), 'c', 'b');

        let left = import_asdf(first.path(), None, None, true).unwrap();
        let right = import_asdf(second.path(), None, None, true).unwrap();
        assert_ne!(left.digest, right.digest);
        assert_ne!(
            left.plan.sources[0].digest.as_ref(),
            right.plan.sources[0].digest.as_ref()
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_config_symlink_cannot_escape_project_root() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join(DEFAULT_CONFIG_PATH), "nodejs 22.11.0\n").unwrap();
        let project = tempfile::tempdir().unwrap();
        symlink(
            outside.path().join(DEFAULT_CONFIG_PATH),
            project.path().join(DEFAULT_CONFIG_PATH),
        )
        .unwrap();

        let error = import_asdf(project.path(), None, None, false).unwrap_err();
        assert!(error.to_string().contains("escapes the project root"));
    }
}
