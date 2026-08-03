from pathlib import Path

path = Path("src/environment.rs")
text = path.read_text()


def replace_once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one match, found {count}: {old[:120]!r}")
    text = text.replace(old, new, 1)


replace_once(
    "use anyhow::{Context, Result, bail};\nuse sha2::{Digest, Sha256};",
    "use anyhow::{Context, Result, bail};\nuse serde::Serialize;\nuse sha2::{Digest, Sha256};",
)

replace_once(
    "#[derive(Debug, Clone)]\nstruct ConfiguredTool {",
    "#[derive(Debug, Clone, Serialize)]\nstruct ConfiguredTool {",
)

replace_once(
    '''#[derive(Debug, Clone)]
struct LockedTool {
    version: String,
    backend: Option<String>,
    checksums: Vec<Checksum>,
    platforms: Vec<String>,
}
''',
    '''#[derive(Debug, Clone, Default, Serialize)]
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
''',
)

replace_once(
    '''    let configured =
        if config_path.file_name().and_then(|name| name.to_str()) == Some(".tool-versions") {
            parse_tool_versions(config_text, &config_relative)?
        } else {
            parse_mise_toml(config_text, &config_relative)?
        };

    let lock_path = resolve_lock_path(&root, &config_path, lock)?;
''',
    '''    let (configured, settings) =
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
''',
)

replace_once(
    '''    let (locked, lock_bytes, lock_relative) = match lock_path {
        Some(path) => {
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
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
''',
    '''    let (locked, lock_relative) = match lock_path {
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
''',
)

replace_once(
    '''        let mut platforms = configured_tool.platforms;
        let mut checksums = Vec::new();
        let resolved = locked_tool.map(|tool| {
            platforms.extend(tool.platforms.iter().cloned());
            checksums.extend(tool.checksums.iter().cloned());
            tool.version.clone()
        });
''',
    '''        let mut platforms = configured_tool.platforms;
        let mut checksums = Vec::new();
        let resolved = locked_tool.map(|tool| {
            for (platform, artifact) in &tool.artifacts {
                platforms.push(platform.clone());
                checksums.extend(artifact.checksum.iter().cloned());
            }
            tool.version.clone()
        });
''',
)

replace_once(
    '''    let source_digest = digest_manager_inputs(&config_bytes, lock_bytes.as_deref());
    plan.sources.push(EnvironmentSource {
''',
    '''    plan.sources.push(EnvironmentSource {
''',
)

replace_once(
    '''fn parse_mise_toml(input: &str, path: &str) -> Result<BTreeMap<String, ConfiguredTool>> {
''',
    '''fn parse_mise_toml(
    input: &str,
    path: &str,
) -> Result<(BTreeMap<String, ConfiguredTool>, MiseSettings)> {
''',
)

replace_once(
    '''    validate_supported_settings(root.get("settings"), path)?;

    let tools = root
''',
    '''    let settings = parse_supported_settings(root.get("settings"), path)?;

    let tools = root
''',
)

replace_once(
    '''    Ok(parsed)
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
''',
    '''    Ok((parsed, settings))
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
''',
)

replace_once(
    '''    let mut checksums = Vec::new();
    let mut platforms = Vec::new();
    if let Some(value) = table.get("platforms") {
''',
    '''    let mut artifacts = BTreeMap::new();
    if let Some(value) = table.get("platforms") {
''',
)

replace_once(
    '''        for (platform, metadata) in platform_table {
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
''',
    '''        for (platform, metadata) in platform_table {
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
''',
)

replace_once(
    '''fn digest_manager_inputs(config: &[u8], lock: Option<&[u8]>) -> Checksum {
    let mut hasher = Sha256::new();
    hasher.update(b"zed-pkg:mise-input:v1\\0");
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
''',
    '''fn verify_frozen_artifacts(
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
            if artifact.checksum.is_none() && artifact.url.is_none() {
                bail!(
                    "mise lock `{lock_path}` has no checksum or URL for `{name}` on `{platform}`"
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
    hasher.update(b"zed-pkg:mise-input:v1\\0");
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(Checksum {
        algorithm: ChecksumAlgorithm::Sha256,
        value: hex::encode(hasher.finalize()),
    })
}
''',
)

append = r'''

    #[test]
    fn normalized_digest_ignores_toml_presentation_but_tracks_semantics() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for root in [first.path(), second.path()] {
            fs::write(
                root.join("mise.toml"),
                if root == first.path() {
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
        fs::write(first.path().join("mise.lock"), first_lock).unwrap();
        fs::write(second.path().join("mise.lock"), second_lock).unwrap();

        let first = import_mise(first.path(), None, None, true).unwrap();
        let second = import_mise(second.path(), None, None, true).unwrap();
        assert_eq!(first.plan.sources[0].digest, second.plan.sources[0].digest);
        assert_eq!(first.digest, second.digest);

        fs::write(
            second.path().join("mise.lock"),
            format!(
                "[[tools.node]]\nversion=\"22.4.0\"\nbackend=\"core:node\"\n[tools.node.platforms.linux-x64]\nurl=\"https://mirror.invalid/node\"\nchecksum=\"{}\"\n[[tools.python]]\nversion=\"3.12.4\"\nbackend=\"core:python\"\n[tools.python.platforms.macos-arm64]\nurl=\"https://example.invalid/python\"\nchecksum=\"{}\"\n",
                checksum('a'),
                checksum('b')
            ),
        )
        .unwrap();
        let changed = import_mise(second.path(), None, None, true).unwrap();
        assert_ne!(first.plan.sources[0].digest, changed.plan.sources[0].digest);
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
'''

stripped = text.rstrip()
if not stripped.endswith("}"):
    raise SystemExit("environment test module does not end in a closing brace")
text = stripped[:-1] + append + "\n}\n"
path.write_text(text)
