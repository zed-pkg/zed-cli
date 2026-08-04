#!/usr/bin/env python3
"""Temporary materializer for current mise flattened platform-key support."""

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


source = Path("src/mise_lock.rs")
text = source.read_text(encoding="utf-8")

if "fn normalize_current_wire_platform_keys(" not in text:
    anchor = "impl MiseLockDocument {\n"
    helpers = r'''fn normalize_current_wire_platform_keys(value: &mut toml::Value, path: &str) -> Result<()> {
    let root = value
        .as_table_mut()
        .with_context(|| format!("current mise lock `{path}` must be a TOML table"))?;
    let Some(tools_value) = root.get_mut("tools") else {
        return Ok(());
    };
    let tools = tools_value
        .as_table_mut()
        .with_context(|| format!("`tools` in current mise lock `{path}` must be a table"))?;

    for (tool_name, value) in tools {
        match value {
            toml::Value::Array(entries) => {
                for (index, entry) in entries.iter_mut().enumerate() {
                    let table = entry.as_table_mut().with_context(|| {
                        format!(
                            "`tools.{tool_name}[{index}]` in current mise lock `{path}` must be a table"
                        )
                    })?;
                    unflatten_platform_keys(table, tool_name, index, path)?;
                }
            }
            toml::Value::Table(table) => {
                unflatten_platform_keys(table, tool_name, 0, path)?;
            }
            _ => bail!(
                "`tools.{tool_name}` in current mise lock `{path}` must be a table or table array"
            ),
        }
    }
    Ok(())
}

fn unflatten_platform_keys(
    table: &mut toml::map::Map<String, toml::Value>,
    tool_name: &str,
    index: usize,
    path: &str,
) -> Result<()> {
    let keys = table
        .keys()
        .filter(|key| key.starts_with("platforms."))
        .cloned()
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return Ok(());
    }
    ensure!(
        !table.contains_key("platforms"),
        "`tools.{tool_name}[{index}]` in current mise lock `{path}` mixes nested `platforms` with current quoted `platforms.<target>` keys"
    );

    let mut platforms = toml::map::Map::new();
    for key in keys {
        let platform = key.strip_prefix("platforms.").unwrap_or_default();
        ensure!(
            !platform.is_empty(),
            "`tools.{tool_name}[{index}]` in current mise lock `{path}` has an empty platform key"
        );
        let metadata = table
            .remove(&key)
            .with_context(|| format!("failed to extract `{key}` from `tools.{tool_name}[{index}]`"))?;
        ensure!(
            platforms.insert(platform.to_string(), metadata).is_none(),
            "duplicate current platform identity `{platform}` in `tools.{tool_name}[{index}]` of `{path}`"
        );
    }
    table.insert("platforms".to_string(), toml::Value::Table(platforms));
    Ok(())
}

fn flatten_current_wire_platform_keys(value: &mut toml::Value, path: &str) -> Result<()> {
    let root = value
        .as_table_mut()
        .with_context(|| format!("normalized current mise lock `{path}` must be a TOML table"))?;
    let Some(tools_value) = root.get_mut("tools") else {
        return Ok(());
    };
    let tools = tools_value
        .as_table_mut()
        .with_context(|| format!("`tools` in normalized current mise lock `{path}` must be a table"))?;

    for (tool_name, value) in tools {
        match value {
            toml::Value::Array(entries) => {
                for (index, entry) in entries.iter_mut().enumerate() {
                    let table = entry.as_table_mut().with_context(|| {
                        format!(
                            "`tools.{tool_name}[{index}]` in normalized current mise lock `{path}` must be a table"
                        )
                    })?;
                    flatten_platform_keys(table, tool_name, index, path)?;
                }
            }
            toml::Value::Table(table) => {
                flatten_platform_keys(table, tool_name, 0, path)?;
            }
            _ => bail!(
                "`tools.{tool_name}` in normalized current mise lock `{path}` must be a table or table array"
            ),
        }
    }
    Ok(())
}

fn flatten_platform_keys(
    table: &mut toml::map::Map<String, toml::Value>,
    tool_name: &str,
    index: usize,
    path: &str,
) -> Result<()> {
    let Some(platforms_value) = table.remove("platforms") else {
        return Ok(());
    };
    let platforms = platforms_value.as_table().with_context(|| {
        format!(
            "`tools.{tool_name}[{index}].platforms` in normalized current mise lock `{path}` must be a table"
        )
    })?;
    let entries = platforms
        .iter()
        .map(|(platform, metadata)| (platform.clone(), metadata.clone()))
        .collect::<Vec<_>>();
    for (platform, metadata) in entries {
        let key = format!("platforms.{platform}");
        ensure!(
            table.insert(key.clone(), metadata).is_none(),
            "duplicate flattened current platform key `{key}` in `tools.{tool_name}[{index}]` of `{path}`"
        );
    }
    Ok(())
}

'''
    text = replace_once(text, anchor, helpers + anchor, "current wire helpers")

old_parse = '''        let document: Self = toml::from_str(input)
            .with_context(|| format!("failed to parse current mise lock `{path}`"))?;
        document.validate(path, mode)?;
'''
new_parse = '''        let mut value: toml::Value = toml::from_str(input)
            .with_context(|| format!("failed to parse current mise lock `{path}`"))?;
        normalize_current_wire_platform_keys(&mut value, path)?;
        let document: Self = value
            .try_into()
            .with_context(|| format!("failed to decode current mise lock `{path}`"))?;
        document.validate(path, mode)?;
'''
if "normalize_current_wire_platform_keys(&mut value, path)?;" not in text:
    text = replace_once(text, old_parse, new_parse, "parse current wire")

old_toml = '''    pub fn to_toml_string(&self) -> Result<String> {
        self.validate("<normalized mise lock>", MiseLockValidationMode::Authoring)?;
        toml::to_string_pretty(&self.normalized())
            .context("failed to serialize normalized current mise lock as TOML")
    }
'''
new_toml = '''    pub fn to_toml_string(&self) -> Result<String> {
        self.validate("<normalized mise lock>", MiseLockValidationMode::Authoring)?;
        let nested = toml::to_string(&self.normalized())
            .context("failed to stage normalized current mise lock as TOML")?;
        let mut value: toml::Value = toml::from_str(&nested)
            .context("failed to stage normalized current mise lock as a TOML value")?;
        flatten_current_wire_platform_keys(&mut value, "<normalized mise lock>")?;
        toml::to_string_pretty(&value)
            .context("failed to serialize normalized current mise lock as TOML")
    }
'''
if "flatten_current_wire_platform_keys(&mut value" not in text[text.index("impl MiseLockDocument {") :]:
    text = replace_once(text, old_toml, new_toml, "serialize current wire")

source.write_text(text, encoding="utf-8")

tests = Path("tests/mise_lock_contract.rs")
text = tests.read_text(encoding="utf-8")
if "pinned_current_mise_wire_fixture_round_trips" not in text:
    addition = r'''

#[test]
fn pinned_current_mise_wire_fixture_round_trips_without_platform_key_loss() {
    let source = include_str!("fixtures/mise-lock/current-actionlint.lock");
    let parsed = MiseLockDocument::parse(
        source,
        "current-actionlint.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    let actionlint = &parsed.tools["actionlint"][0];
    assert_eq!(actionlint.platforms.len(), 2);
    assert!(actionlint.platforms.contains_key("linux-x64"));
    assert!(actionlint.platforms.contains_key("macos-arm64"));

    let rendered = parsed.to_toml_string().unwrap();
    assert!(rendered.contains("[tools.actionlint.\"platforms.linux-x64\"]"));
    assert!(rendered.contains("[tools.actionlint.\"platforms.macos-arm64\"]"));
    assert!(!rendered.contains("[tools.actionlint.platforms.linux-x64]"));

    let reparsed = MiseLockDocument::parse(
        &rendered,
        "rendered-current-actionlint.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    assert_eq!(parsed.normalized(), reparsed.normalized());
    assert_eq!(
        parsed.semantic_digest_sha256().unwrap(),
        reparsed.semantic_digest_sha256().unwrap()
    );
}

#[test]
fn provenance_mutation_in_current_wire_changes_semantic_identity() {
    let source = include_str!("fixtures/mise-lock/current-actionlint.lock");
    let github = MiseLockDocument::parse(
        source,
        "current-actionlint.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    let cosign_source = source.replacen(
        "provenance = \"github-attestations\"",
        "provenance = \"cosign\"",
        1,
    );
    let cosign = MiseLockDocument::parse(
        &cosign_source,
        "current-actionlint-cosign.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap();
    assert_ne!(
        github.semantic_digest_sha256().unwrap(),
        cosign.semantic_digest_sha256().unwrap()
    );
}

#[test]
fn mixed_nested_and_current_platform_encodings_fail_closed() {
    let source = format!(
        r#"
[[tools.node]]
version = "22.4.0"
[tools.node.platforms.linux-x64]
checksum = "sha256:{SHA256_A}"
[tools.node."platforms.macos-arm64"]
checksum = "sha256:{SHA256_B}"
"#
    );
    let error = MiseLockDocument::parse(
        &source,
        "mixed.lock",
        MiseLockValidationMode::FrozenPortable,
    )
    .unwrap_err();
    assert!(error.to_string().contains("mixes nested `platforms`"));
}
'''
    text += addition
tests.write_text(text, encoding="utf-8")

docs = Path("docs/mise-lock-contract.md")
text = docs.read_text(encoding="utf-8")
if "## Current wire format" not in text:
    anchor = "## Validation modes\n"
    section = '''## Current wire format

Current mise serializes platform identities as quoted literal keys beneath each tool identity:

```toml
[[tools.actionlint]]
version = "1.7.12"
backend = "aqua:rhysd/actionlint"

[tools.actionlint."platforms.linux-x64"]
checksum = "sha256:..."
url = "https://..."
url_api = "https://api.github.com/..."
provenance = "github-attestations"
```

The parser accepts this current wire form and the earlier nested compatibility form, but rejects a single identity that mixes both encodings. Deterministic TOML output always uses the quoted current `"platforms.<target>"` form. A fixture copied from mise commit `72379d0c459808f980a037065ac9c39a60032280` proves parse, deterministic render, reparse, and semantic-digest equality without invoking mise.

'''
    text = replace_once(text, anchor, section + anchor, "wire format documentation")
docs.write_text(text, encoding="utf-8")
