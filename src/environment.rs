//! Project-local developer-environment interoperability.
//!
//! This module deliberately starts with a read-only mise adapter. It imports
//! the supported tool/lock subset into the shared `EnvironmentPlan` contract
//! without invoking mise, loading parent/global configuration, executing
//! hooks, or mutating either manager's files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::environment::{
    ActivationPolicy, Checksum, ChecksumAlgorithm, EnvironmentManager, EnvironmentPlan,
    EnvironmentSource, EnvironmentValidationMode, ToolRequirement,
};

const MISE_CONFIG_CANDIDATES: &[&str] = &["mise.toml", ".mise.toml", ".tool-versions"];

#[derive(Debug, Clone)]
pub struct ImportedMiseEnvironment {
    pub plan: EnvironmentPlan,
    pub config_path: String,
    pub lock_path: Option<String>,
    pub digest: String,
}

#[derive(Debug, Clone)]
struct ConfiguredTool {
    requirement: String,
    provider: Option<String>,
    backend: Option<String>,
    platforms: Vec<String>,
}

#[derive(Debug, Clone)]
struct LockedTool {
    version: String,
    backend: Option<String>,
    checksums: Vec<Checksum>,
    platforms: Vec<String>,
}

pub fn import_mise(
    cwd: &Path,
    config: Option<&Path>,
    lock: Option<&Path>,
    frozen: bool,
) -> Result<ImportedMiseEnvironment> {
    let root = cwd
        .canonicalize()
        .with_context(|| format!("failed to resolve project root {}", cwd.display()))?;
    let config_path = match config {
        Some(path) => resolve_project_file(&root, path, "mise config")?,
        None => discover_mise_config(&root)?,
    };
    let config_bytes = fs::read(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config_text = std::str::from_utf8(&config_bytes)
        .with_context(|| format!("{} is not UTF-8", config_path.display()))?;
    let config_relative = project_relative_path(&root, &config_path)?;

    let configured = if config_path.file_name().and_then(|name| name.to_str())
        == Some(".tool-versions")
    {
        parse_tool_versions(config_text, &config_relative)?
    } else {
        parse_mise_toml(config_text, &config_relative)?
    };

    let lock_path = resolve_lock_path(&root, &config_path, lock)?;
    if frozen && lock_path.is_none() {
        bail!(
            "frozen mise import requires a project-local lockfile next to `{config_relative}`; run `mise lock` and commit the resulting lockfile"
        );
    }

    let (locked, lock_bytes, lock_relative) = match lock_path {
        Some(path) => {
            let bytes = fs::read(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let text = std::str::from_utf8(&bytes)
                .with_context(|| format!("{} is not UTF-8", path.display()))?;
            let relative = project_relative_path(&root, &path)?;
            (
                parse_mise_lock(text, &relative)?,
                Some(bytes),
                Some(relative),
            )
        }
        None => (BTreeMap::new(), None, None),
    };

    if frozen {
        verify_lock_coverage(&configured, &locked, lock_relative.as_deref())?;
    }

    let mut plan = EnvironmentPlan {
        activation: ActivationPolicy::FrozenInstall,
        ..EnvironmentPlan::default()
    };

    for (name, configured_tool) in configured {
        let locked_tool = locked.get(&name);
        let backend = locked_tool
            .and_then(|tool| tool.backend.clone())
            .or(configured_tool.backend);
        let provider = backend
            .as_deref()
            .and_then(|value| value.split_once(':').map(|(provider, _)| provider.to_string()))
            .or(configured_tool.provider);
        let mut platforms = configured_tool.platforms;
        let mut checksums = Vec::new();
        let resolved = locked_tool.map(|tool| {
            platforms.extend(tool.platforms.iter().cloned());
            checksums.extend(tool.checksums.iter().cloned());
            tool.version.clone()
        });

        plan.tools.insert(
            name,
            ToolRequirement {
                requirement: configured_tool.requirement,
                resolved,
                provider,
                backend,
                source: None,
                checksums,
                platforms,
            },
        );
    }

    let source_digest = digest_manager_inputs(&config_bytes, lock_bytes.as_deref());
    plan.sources.push(EnvironmentSource {
        manager: EnvironmentManager::Mise,
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
        .context("mise environment did not satisfy the shared environment contract")?;

    let canonical = plan.canonical_json_bytes()?;
    let digest = hex::encode(Sha256::digest(&canonical));

    Ok(ImportedMiseEnvironment {
        plan: plan.normalized(),
        config_path: config_relative,
        lock_path: lock_relative,
        digest,
    })
}

pub fn print_import(imported: &ImportedMiseEnvironment, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&imported.plan)?);
        return Ok(());
    }

    println!("manager: mise");
    println!("config: {}", imported.config_path);
    println!(
        "lock: {}",
        imported.lock_path.as_deref().unwrap_or("<none; authoring mode>")
    );
    println!("tools: {}", imported.plan.tools.len());
    println!("environment-plan-sha256: {}", imported.digest);
    Ok(())
}

pub fn print_verification(imported: &ImportedMiseEnvironment, json: bool) -> Result<()> {
    if json {
        let output = serde_json::json!({
            "manager": "mise",
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
        "verified mise environment: {} tool(s), digest {}",
        imported.plan.tools.len(),
        imported.digest
    );
    Ok(())
}

fn discover_mise_config(root: &Path) -> Result<PathBuf> {
    let candidates = MISE_CONFIG_CANDIDATES
        .iter()
        .map(|name| root.join(name))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();

    match candidates.as_slice() {
        [] => bail!(
            "no project-local mise configuration found; expected one of {}",
            MISE_CONFIG_CANDIDATES.join(", ")
        ),
        [path] => resolve_project_file(root, path, "mise config"),
        paths => {
            let names = paths
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "multiple project-local mise configurations found ({names}); pass --config explicitly so precedence is not guessed"
            )
        }
    }
}

fn resolve_lock_path(
    root: &Path,
    config_path: &Path,
    requested: Option<&Path>,
) -> Result<Option<PathBuf>> {
    if let Some(path) = requested {
        return resolve_project_file(root, path, "mise lockfile").map(Some);
    }
    if config_path.file_name().and_then(|name| name.to_str()) == Some(".tool-versions") {
        return Ok(None);
    }

    let candidate = config_path.with_extension("lock");
    if candidate.is_file() {
        resolve_project_file(root, &candidate, "mise lockfile").map(Some)
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
                    .context("environment paths must be valid UTF-8")?
                    .to_string(),
            ),
            _ => bail!("environment paths must be normalized and project-relative"),
        }
    }
    if parts.is_empty() {
        bail!("environment path must name a project file");
    }
    Ok(parts.join("/"))
}

fn parse_mise_toml(input: &str, path: &str) -> Result<BTreeMap<String, ConfiguredTool>> {
    let value: toml::Value = toml::from_str(input)
        .with_context(|| format!("failed to parse mise configuration `{path}`"))?;
    let root = value
        .as_table()
        .with_context(|| format!("mise configuration `{path}` must be a TOML table"))?;

    for key in root.keys() {
        if !matches!(key.as_str(), "tools" | "settings") {
            bail!(
                "unsupported mise field `{key}` in `{path}`; this read-only adapter currently imports tools and lock settings only and will not silently discard env, tasks, hooks, plugins, includes, or templates"
            );
        }
    }
    validate_supported_settings(root.get("settings"), path)?;

    let tools = root
        .get("tools")
        .with_context(|| format!("mise configuration `{path}` has no [tools] table"))?
        .as_table()
        .with_context(|| format!("`tools` in `{path}` must be a table"))?;
    if tools.is_empty() {
        bail!("mise configuration `{path}` has an empty [tools] table");
    }

    let mut parsed = BTreeMap::new();
    for (name, value) in tools {
        let tool = parse_configured_tool(name, value, path)?;
        if parsed.insert(name.clone(), tool).is_some() {
            bail!("duplicate mise tool `{name}` in `{path}`");
        }
    }
    Ok(parsed)
}

fn validate_supported_settings(value: Option<&toml::Value>, path: &str) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let settings = value
        .as_table()
        .with_context(|| format!("`settings` in `{path}` must be a table"))?;
    for key in settings.keys() {
        if !matches!(key.as_str(), "lockfile" | "locked" | "lockfile_platforms") {
            bail!(
                "unsupported mise setting `settings.{key}` in `{path}`; pass only lockfile-related settings to the initial adapter"
            );
        }
    }
    Ok(())
}

fn parse_configured_tool(name: &str, value: &toml::Value, path: &str) -> Result<ConfiguredTool> {
    let (requirement, platforms) = match value {
        toml::Value::String(version) => (version.clone(), Vec::new()),
        toml::Value::Table(table) => {
            for key in table.keys() {
                if !matches!(key.as_str(), "version" | "os") {
                    bail!(
                        "unsupported mise tool field `tools.{name}.{key}` in `{path}`; install hooks/options are intentionally not executed or discarded"
                    );
                }
            }
            let version = table
                .get("version")
                .and_then(toml::Value::as_str)
                .with_context(|| {
                    format!("`tools.{name}.version` in `{path}` must be a string")
                })?
                .to_string();
            let platforms = parse_os_constraint(table.get("os"), name, path)?;
            (version, platforms)
        }
        toml::Value::Array(_) => bail!(
            "mise tool `tools.{name}` in `{path}` selects multiple versions; EnvironmentPlan v1 represents one resolved identity per tool, so this cannot be imported losslessly yet"
        ),
        _ => bail!(
            "mise tool `tools.{name}` in `{path}` must be a version string or a table containing `version`"
        ),
    };
    if requirement.trim().is_empty() {
        bail!("mise tool `tools.{name}` in `{path}` has an empty version requirement");
    }

    let (provider, backend) = name
        .split_once(':')
        .map(|(provider, _)| (Some(provider.to_string()), Some(name.to_string())))
        .unwrap_or((None, None));
    Ok(ConfiguredTool {
        requirement,
        provider,
        backend,
        platforms,
    })
}

fn parse_os_constraint(value: Option<&toml::Value>, name: &str, path: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        toml::Value::String(os) => Ok(vec![os.clone()]),
        toml::Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value.as_str().map(ToOwned::to_owned).with_context(|| {
                    format!("`tools.{name}.os[{index}]` in `{path}` must be a string")
                })
            })
            .collect(),
        _ => bail!("`tools.{name}.os` in `{path}` must be a string or string array"),
    }
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
                "`{path}` line {} must contain exactly one tool and one version; multiple versions cannot be represented losslessly by EnvironmentPlan v1",
                index + 1
            );
        }
        let name = fields[0].to_string();
        let configured = ConfiguredTool {
            requirement: fields[1].to_string(),
            provider: None,
            backend: None,
            platforms: Vec::new(),
        };
        if tools.insert(name.clone(), configured).is_some() {
            bail!("duplicate tool `{name}` in `{path}`");
        }
    }
    if tools.is_empty() {
        bail!("`{path}` contains no tool versions");
    }
    Ok(tools)
}

fn parse_mise_lock(input: &str, path: &str) -> Result<BTreeMap<String, LockedTool>> {
    let value: toml::Value = toml::from_str(input)
        .with_context(|| format!("failed to parse mise lockfile `{path}`"))?;
    let root = value
        .as_table()
        .with_context(|| format!("mise lockfile `{path}` must be a TOML table"))?;
    for key in root.keys() {
        if key != "tools" {
            bail!(
                "unsupported mise lockfile section `{key}` in `{path}`; transitive/backend-specific lock sections require a later schema extension"
            );
        }
    }
    let Some(tools_value) = root.get("tools") else {
        return Ok(BTreeMap::new());
    };
    let tools = tools_value
        .as_table()
        .with_context(|| format!("`tools` in mise lockfile `{path}` must be a table"))?;

    let mut parsed = BTreeMap::new();
    for (name, value) in tools {
        let entries = match value {
            toml::Value::Array(entries) => entries.as_slice(),
            toml::Value::Table(_) => std::slice::from_ref(value),
            _ => bail!("lock entry `tools.{name}` in `{path}` must be a table array"),
        };
        if entries.len() != 1 {
            bail!(
                "lock entry `tools.{name}` in `{path}` has {} identities; EnvironmentPlan v1 cannot preserve option-dependent or multi-version identities losslessly",
                entries.len()
            );
        }
        let locked = parse_locked_tool(name, &entries[0], path)?;
        parsed.insert(name.clone(), locked);
    }
    Ok(parsed)
}

fn parse_locked_tool(name: &str, value: &toml::Value, path: &str) -> Result<LockedTool> {
    let table = value
        .as_table()
        .with_context(|| format!("lock entry `tools.{name}` in `{path}` must be a table"))?;
    for key in table.keys() {
        if !matches!(key.as_str(), "version" | "backend" | "options" | "platforms") {
            bail!("unsupported lock field `tools.{name}.{key}` in `{path}`");
        }
    }
    if let Some(options) = table.get("options") {
        let empty = options.as_table().is_some_and(toml::map::Map::is_empty);
        if !empty {
            bail!(
                "lock entry `tools.{name}.options` in `{path}` changes artifact identity and cannot be represented losslessly by EnvironmentPlan v1"
            );
        }
    }

    let version = table
        .get("version")
        .and_then(toml::Value::as_str)
        .with_context(|| format!("lock entry `tools.{name}.version` in `{path}` must be a string"))?
        .to_string();
    let backend = table
        .get("backend")
        .map(|value| {
            value.as_str().map(ToOwned::to_owned).with_context(|| {
                format!("lock entry `tools.{name}.backend` in `{path}` must be a string")
            })
        })
        .transpose()?;

    let mut checksums = Vec::new();
    let mut platforms = Vec::new();
    if let Some(value) = table.get("platforms") {
        let platform_table = value.as_table().with_context(|| {
            format!("lock entry `tools.{name}.platforms` in `{path}` must be a table")
        })?;
        for (platform, metadata) in platform_table {
            platforms.push(platform.clone());
            let metadata = metadata.as_table().with_context(|| {
                format!("lock metadata `tools.{name}.platforms.{platform}` must be a table")
            })?;
            for key in metadata.keys() {
                if !matches!(key.as_str(), "checksum" | "size" | "url") {
                    bail!(
                        "unsupported lock field `tools.{name}.platforms.{platform}.{key}` in `{path}`"
                    );
                }
            }
            if let Some(value) = metadata.get("checksum") {
                let checksum = value.as_str().with_context(|| {
                    format!(
                        "lock checksum `tools.{name}.platforms.{platform}.checksum` in `{path}` must be a string"
                    )
                })?;
                checksums.push(parse_checksum(checksum, name, platform, path)?);
            }
        }
    }

    Ok(LockedTool {
        version,
        backend,
        checksums,
        platforms,
    })
}

fn parse_checksum(value: &str, tool: &str, platform: &str, path: &str) -> Result<Checksum> {
    let (algorithm, digest) = value.split_once(':').with_context(|| {
        format!(
            "lock checksum for `{tool}` on `{platform}` in `{path}` must use algorithm:digest syntax"
        )
    })?;
    let algorithm = match algorithm.to_ascii_lowercase().as_str() {
        "sha256" => ChecksumAlgorithm::Sha256,
        "sha512" => ChecksumAlgorithm::Sha512,
        "blake3" => ChecksumAlgorithm::Blake3,
        other => bail!(
            "unsupported checksum algorithm `{other}` for `{tool}` on `{platform}` in `{path}`"
        ),
    };
    let expected = match algorithm {
        ChecksumAlgorithm::Sha256 | ChecksumAlgorithm::Blake3 => 64,
        ChecksumAlgorithm::Sha512 => 128,
    };
    if digest.len() != expected || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "invalid checksum for `{tool}` on `{platform}` in `{path}`: expected {expected} hexadecimal characters"
        );
    }
    Ok(Checksum {
        algorithm,
        value: digest.to_ascii_lowercase(),
    })
}

fn verify_lock_coverage(
    configured: &BTreeMap<String, ConfiguredTool>,
    locked: &BTreeMap<String, LockedTool>,
    lock_path: Option<&str>,
) -> Result<()> {
    let lock_path = lock_path.unwrap_or("<missing lockfile>");
    let configured_names = configured.keys().cloned().collect::<BTreeSet<_>>();
    let locked_names = locked.keys().cloned().collect::<BTreeSet<_>>();
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
            "mise lock/config drift in `{lock_path}`: missing locked tools [{}]; extra locked tools [{}]",
            missing.join(", "),
            extra.join(", ")
        );
    }
    Ok(())
}

fn digest_manager_inputs(config: &[u8], lock: Option<&[u8]>) -> Checksum {
    let mut hasher = Sha256::new();
    hasher.update(b"zed-pkg:mise-input:v1\0");
    hasher.update((config.len() as u64).to_be_bytes());
    hasher.update(config);
    match lock {
        Some(lock) => {
            hasher.update([1]);
            hasher.update((lock.len() as u64).to_be_bytes());
            hasher.update(lock);
        }
        None => hasher.update([0]),
    }
    Checksum {
        algorithm: ChecksumAlgorithm::Sha256,
        value: hex::encode(hasher.finalize()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checksum(digit: char) -> String {
        format!("sha256:{}", digit.to_string().repeat(64))
    }

    #[test]
    fn imports_locked_scalar_and_table_tools() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("mise.toml"),
            r#"
[settings]
lockfile = true

[tools]
node = "22"
python = { version = "3.12", os = ["linux", "macos"] }
"#,
        )
        .unwrap();
        fs::write(
            temp.path().join("mise.lock"),
            format!(
                r#"
[[tools.node]]
version = "22.4.0"
backend = "core:node"

[tools.node.platforms.linux-x64]
checksum = "{}"
size = 123
url = "https://example.invalid/node.tar.xz"

[[tools.python]]
version = "3.12.4"
backend = "core:python"

[tools.python.platforms.macos-arm64]
checksum = "{}"
"#,
                checksum('a'),
                checksum('b')
            ),
        )
        .unwrap();

        let imported = import_mise(temp.path(), None, None, true).unwrap();
        assert_eq!(imported.config_path, "mise.toml");
        assert_eq!(imported.lock_path.as_deref(), Some("mise.lock"));
        assert_eq!(
            imported.plan.tools["node"].resolved.as_deref(),
            Some("22.4.0")
        );
        assert_eq!(
            imported.plan.tools["python"].resolved.as_deref(),
            Some("3.12.4")
        );
        assert_eq!(imported.plan.tools["node"].checksums.len(), 1);
        assert_eq!(imported.digest.len(), 64);
    }

    #[test]
    fn frozen_mode_requires_lock_coverage() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        let error = import_mise(temp.path(), None, None, true).unwrap_err();
        assert!(error.to_string().contains("requires a project-local lockfile"));
    }

    #[test]
    fn discovery_refuses_ambiguous_project_configs() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        fs::write(temp.path().join(".mise.toml"), "[tools]\npython = \"3.12\"\n").unwrap();
        let error = import_mise(temp.path(), None, None, false).unwrap_err();
        assert!(error.to_string().contains("multiple project-local"));
    }

    #[test]
    fn unsupported_semantics_fail_instead_of_disappearing() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("mise.toml"),
            "[tools]\nnode = \"22\"\n[tasks.test]\nrun = \"cargo test\"\n",
        )
        .unwrap();
        let error = import_mise(temp.path(), None, None, false).unwrap_err();
        assert!(error.to_string().contains("unsupported mise field `tasks`"));
    }

    #[test]
    fn tool_versions_is_supported_only_in_authoring_mode() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(".tool-versions"),
            "node 22.4.0\npython 3.12.4 # pinned\n",
        )
        .unwrap();
        let imported = import_mise(temp.path(), None, None, false).unwrap();
        assert_eq!(imported.plan.tools.len(), 2);
        assert!(imported.plan.tools["node"].resolved.is_none());
        assert!(import_mise(temp.path(), None, None, true).is_err());
    }
}
