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
use serde::Serialize;
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

#[derive(Debug, Clone, Serialize)]
struct ConfiguredTool {
    requirement: String,
    provider: Option<String>,
    backend: Option<String>,
    platforms: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct MiseSettings {
    lockfile: Option<bool>,
    locked: Option<bool>,
    lockfile_platforms: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LockedArtifact {
    checksum: Option<Checksum>,
    size: Option<u64>,
    url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LockedTool {
    version: String,
    backend: Option<String>,
    artifacts: BTreeMap<String, LockedArtifact>,
}

#[derive(Serialize)]
struct NormalizedMiseInputs<'a> {
    schema: u32,
    settings: &'a MiseSettings,
    tools: &'a BTreeMap<String, ConfiguredTool>,
    lock: Option<&'a BTreeMap<String, LockedTool>>,
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

    let (configured, settings) =
        if config_path.file_name().and_then(|name| name.to_str()) == Some(".tool-versions") {
            (
                parse_tool_versions(config_text, &config_relative)?,
                MiseSettings::default(),
            )
        } else {
            parse_mise_toml(config_text, &config_relative)?
        };

    if settings.locked == Some(true) {
        bail!(
            "`settings.locked = true` in `{config_relative}` has global mise scope and cannot be verified without importing user-global tools; keep the Zed adapter project-local and use `zed env verify mise --frozen` instead"
        );
    }
    if frozen && settings.lockfile == Some(false) {
        bail!(
            "frozen mise import cannot use `{config_relative}` because `settings.lockfile = false` explicitly disables the manager lock"
        );
    }

    let lock_path = resolve_lock_path(&root, &config_path, lock)?;
    if frozen && lock_path.is_none() {
        bail!(
            "frozen mise import requires a project-local lockfile next to `{config_relative}`; run `mise lock` and commit the resulting lockfile"
        );
    }

    let (locked, lock_relative) = match lock_path {
        Some(path) => {
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let text = std::str::from_utf8(&bytes)
                .with_context(|| format!("{} is not UTF-8", path.display()))?;
            let relative = project_relative_path(&root, &path)?;
            (parse_mise_lock(text, &relative)?, Some(relative))
        }
        None => (BTreeMap::new(), None),
    };

    if frozen {
        verify_lock_coverage(&configured, &locked, lock_relative.as_deref())?;
        verify_frozen_artifacts(&locked, lock_relative.as_deref())?;
    }

    let source_digest = digest_manager_inputs(
        &settings,
        &configured,
        lock_relative.as_ref().map(|_| &locked),
    )?;

    let mut plan = EnvironmentPlan {
        platforms: settings.lockfile_platforms.clone(),
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
            .and_then(|value| {
                value
                    .split_once(':')
                    .map(|(provider, _)| provider.to_string())
            })
            .or(configured_tool.provider);
        let mut platforms = configured_tool.platforms;
        let mut checksums = Vec::new();
        let resolved = locked_tool.map(|tool| {
            for (platform, artifact) in &tool.artifacts {
                platforms.push(platform.clone());
                checksums.extend(artifact.checksum.iter().cloned());
            }
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
        imported
            .lock_path
            .as_deref()
            .unwrap_or("<none; authoring mode>")
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
        bail!(
            "{kind} does not exist or is not a file: {}",
            candidate.display()
        );
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

fn parse_mise_toml(
    input: &str,
    path: &str,
) -> Result<(BTreeMap<String, ConfiguredTool>, MiseSettings)> {
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
    let settings = parse_supported_settings(root.get("settings"), path)?;

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
    Ok((parsed, settings))
}

fn parse_supported_settings(value: Option<&toml::Value>, path: &str) -> Result<MiseSettings> {
    let Some(value) = value else {
        return Ok(MiseSettings::default());
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

    let lockfile = settings
        .get("lockfile")
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("`settings.lockfile` in `{path}` must be a boolean"))
        })
        .transpose()?;
    let locked = settings
        .get("locked")
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("`settings.locked` in `{path}` must be a boolean"))
        })
        .transpose()?;
    let mut lockfile_platforms = settings
        .get("lockfile_platforms")
        .map(|value| {
            value
                .as_array()
                .with_context(|| {
                    format!("`settings.lockfile_platforms` in `{path}` must be a string array")
                })?
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value.as_str().map(ToOwned::to_owned).with_context(|| {
                        format!(
                            "`settings.lockfile_platforms[{index}]` in `{path}` must be a string"
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    for platform in &lockfile_platforms {
        if platform.trim().is_empty()
            || platform.trim() != platform
            || platform.chars().any(|character| character.is_control())
        {
            bail!(
                "`settings.lockfile_platforms` in `{path}` contains invalid platform `{platform}`"
            );
        }
    }
    lockfile_platforms.sort();
    lockfile_platforms.dedup();

    Ok(MiseSettings {
        lockfile,
        locked,
        lockfile_platforms,
    })
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
                .with_context(|| format!("`tools.{name}.version` in `{path}` must be a string"))?
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

fn strip_tool_versions_comment(line: &str) -> &str {
    for (index, character) in line.char_indices() {
        if character != '#' {
            continue;
        }
        let begins_comment = index == 0
            || line[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if begins_comment {
            return &line[..index];
        }
    }
    line
}

fn parse_tool_versions(input: &str, path: &str) -> Result<BTreeMap<String, ConfiguredTool>> {
    let mut tools = BTreeMap::new();
    for (index, original) in input.lines().enumerate() {
        let line = strip_tool_versions_comment(original).trim();
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
    let value: toml::Value =
        toml::from_str(input).with_context(|| format!("failed to parse mise lockfile `{path}`"))?;
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
        if !matches!(
            key.as_str(),
            "version" | "backend" | "options" | "platforms"
        ) {
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

    let mut artifacts = BTreeMap::new();
    if let Some(value) = table.get("platforms") {
        let platform_table = value.as_table().with_context(|| {
            format!("lock entry `tools.{name}.platforms` in `{path}` must be a table")
        })?;
        for (platform, metadata) in platform_table {
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
            let checksum = metadata
                .get("checksum")
                .map(|value| {
                    let checksum = value.as_str().with_context(|| {
                        format!(
                            "lock checksum `tools.{name}.platforms.{platform}.checksum` in `{path}` must be a string"
                        )
                    })?;
                    parse_checksum(checksum, name, platform, path)
                })
                .transpose()?;
            let size = metadata
                .get("size")
                .map(|value| {
                    let size = value.as_integer().with_context(|| {
                        format!(
                            "lock size `tools.{name}.platforms.{platform}.size` in `{path}` must be a non-negative integer"
                        )
                    })?;
                    u64::try_from(size).with_context(|| {
                        format!(
                            "lock size `tools.{name}.platforms.{platform}.size` in `{path}` must be non-negative"
                        )
                    })
                })
                .transpose()?;
            let url = metadata
                .get("url")
                .map(|value| {
                    let url = value.as_str().with_context(|| {
                        format!(
                            "lock URL `tools.{name}.platforms.{platform}.url` in `{path}` must be a string"
                        )
                    })?;
                    let url = url.trim();
                    if url.is_empty() || url.chars().any(|character| character.is_control()) {
                        bail!(
                            "lock URL `tools.{name}.platforms.{platform}.url` in `{path}` must be non-empty and contain no control characters"
                        );
                    }
                    Ok(url.to_string())
                })
                .transpose()?;
            artifacts.insert(
                platform.clone(),
                LockedArtifact {
                    checksum,
                    size,
                    url,
                },
            );
        }
    }

    Ok(LockedTool {
        version,
        backend,
        artifacts,
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

fn verify_frozen_artifacts(
    locked: &BTreeMap<String, LockedTool>,
    lock_path: Option<&str>,
) -> Result<()> {
    let lock_path = lock_path.unwrap_or("<missing lockfile>");
    for (name, tool) in locked {
        if tool.artifacts.is_empty() {
            bail!(
                "mise lock `{lock_path}` gives `{name}` an exact version but no platform artifact identity; frozen-portable verification requires at least one checksum or immutable URL"
            );
        }
        for (platform, artifact) in &tool.artifacts {
            if artifact.checksum.is_none() {
                bail!(
                    "mise lock `{lock_path}` has no cryptographic checksum for `{name}` on `{platform}`; a URL alone is provenance, not immutable artifact identity"
                );
            }
        }
    }
    Ok(())
}

fn digest_manager_inputs(
    settings: &MiseSettings,
    tools: &BTreeMap<String, ConfiguredTool>,
    lock: Option<&BTreeMap<String, LockedTool>>,
) -> Result<Checksum> {
    let normalized = NormalizedMiseInputs {
        schema: 1,
        settings,
        tools,
        lock,
    };
    let bytes = serde_json::to_vec(&normalized)
        .context("failed to serialize normalized mise manager state")?;
    let mut hasher = Sha256::new();
    hasher.update(b"zed-pkg:mise-input:v1\0");
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(Checksum {
        algorithm: ChecksumAlgorithm::Sha256,
        value: hex::encode(hasher.finalize()),
    })
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
        assert!(
            error
                .to_string()
                .contains("requires a project-local lockfile")
        );
    }

    #[test]
    fn discovery_refuses_ambiguous_project_configs() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        fs::write(
            temp.path().join(".mise.toml"),
            "[tools]\npython = \"3.12\"\n",
        )
        .unwrap();
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

    #[test]
    fn tool_versions_preserves_hash_inside_version_token() {
        let parsed = parse_tool_versions(
            "custom ref:feature#anchor # trailing comment\n# full-line comment\n",
            ".tool-versions",
        )
        .unwrap();
        assert_eq!(parsed["custom"].requirement, "ref:feature#anchor");
    }

    #[test]
    fn normalized_digest_ignores_toml_presentation_but_tracks_semantics() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        for root in [first_dir.path(), second_dir.path()] {
            fs::write(
                root.join("mise.toml"),
                if root == first_dir.path() {
                    "[settings]\nlockfile_platforms = [\"macos-arm64\", \"linux-x64\"]\nlockfile = true\n[tools]\npython = \"3.12\"\nnode = \"22\"\n"
                } else {
                    "[tools]\nnode=\"22\"\npython=\"3.12\"\n\n[settings]\nlockfile=true\nlockfile_platforms=[\"linux-x64\",\"macos-arm64\"]\n"
                },
            )
            .unwrap();
        }
        let first_lock = format!(
            "[[tools.python]]\nbackend=\"core:python\"\nversion=\"3.12.4\"\n[tools.python.platforms.macos-arm64]\nchecksum=\"{}\"\nurl=\"https://example.invalid/python\"\n[[tools.node]]\nversion=\"22.4.0\"\nbackend=\"core:node\"\n[tools.node.platforms.linux-x64]\nurl=\"https://example.invalid/node\"\nchecksum=\"{}\"\n",
            checksum('b'),
            checksum('a')
        );
        let second_lock = format!(
            "[[tools.node]]\nbackend = \"core:node\"\nversion = \"22.4.0\"\n[tools.node.platforms.linux-x64]\nchecksum = \"{}\"\nurl = \"https://example.invalid/node\"\n\n[[tools.python]]\nversion = \"3.12.4\"\nbackend = \"core:python\"\n[tools.python.platforms.macos-arm64]\nurl = \"https://example.invalid/python\"\nchecksum = \"{}\"\n",
            checksum('a'),
            checksum('b')
        );
        fs::write(first_dir.path().join("mise.lock"), first_lock).unwrap();
        fs::write(second_dir.path().join("mise.lock"), second_lock).unwrap();

        let first_imported = import_mise(first_dir.path(), None, None, true).unwrap();
        let second_imported = import_mise(second_dir.path(), None, None, true).unwrap();
        assert_eq!(
            first_imported.plan.sources[0].digest,
            second_imported.plan.sources[0].digest
        );
        assert_eq!(first_imported.digest, second_imported.digest);

        fs::write(
            second_dir.path().join("mise.lock"),
            format!(
                "[[tools.node]]\nversion=\"22.4.0\"\nbackend=\"core:node\"\n[tools.node.platforms.linux-x64]\nurl=\"https://mirror.invalid/node\"\nchecksum=\"{}\"\n[[tools.python]]\nversion=\"3.12.4\"\nbackend=\"core:python\"\n[tools.python.platforms.macos-arm64]\nurl=\"https://example.invalid/python\"\nchecksum=\"{}\"\n",
                checksum('a'),
                checksum('b')
            ),
        )
        .unwrap();
        let changed = import_mise(second_dir.path(), None, None, true).unwrap();
        assert_ne!(
            first_imported.plan.sources[0].digest,
            changed.plan.sources[0].digest
        );
    }

    #[test]
    fn lock_settings_are_typed_and_global_locked_mode_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("mise.toml"),
            "[settings]\nlockfile = \"yes\"\n[tools]\nnode = \"22\"\n",
        )
        .unwrap();
        let error = import_mise(temp.path(), None, None, false).unwrap_err();
        assert!(error.to_string().contains("must be a boolean"));

        fs::write(
            temp.path().join("mise.toml"),
            "[settings]\nlocked = true\n[tools]\nnode = \"22\"\n",
        )
        .unwrap();
        let error = import_mise(temp.path(), None, None, false).unwrap_err();
        assert!(error.to_string().contains("global mise scope"));
    }

    #[test]
    fn frozen_mode_requires_artifact_identity() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        fs::write(
            temp.path().join("mise.lock"),
            "[[tools.node]]\nversion = \"22.4.0\"\nbackend = \"core:node\"\n",
        )
        .unwrap();
        let error = import_mise(temp.path(), None, None, true).unwrap_err();
        assert!(error.to_string().contains("no platform artifact identity"));
    }
}
