use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::paths::MANIFEST_FILE;

/// Read the optional [overrides.path] table without requiring a newer
/// zed-interfaces crate at this parsing boundary. The shared interface owns the
/// schema; this compatibility reader keeps the CLI rollout contract-first.
pub(crate) fn read(project: &Path) -> Result<BTreeMap<String, String>> {
    let path = project.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let document: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let Some(table) = document
        .get("overrides")
        .and_then(|value| value.get("path"))
    else {
        return Ok(BTreeMap::new());
    };
    let table = table
        .as_table()
        .context("[overrides.path] must be a TOML table")?;
    let mut out = BTreeMap::new();
    for (package, value) in table {
        crate::ops::split_key(package)?;
        let raw = value
            .as_str()
            .with_context(|| format!("[overrides.path].{package} must be a string"))?;
        validate_raw(package, raw)?;
        out.insert(package.clone(), raw.to_string());
    }
    Ok(out)
}

fn validate_raw(package: &str, raw: &str) -> Result<()> {
    if raw.trim().is_empty() {
        bail!("local path override for `{package}` must not be empty");
    }
    if raw.chars().any(char::is_control) {
        bail!("local path override for `{package}` contains control characters");
    }
    if raw.contains("$(") || raw.contains('`') {
        bail!(
            "local path override for `{package}` permits only $VAR and ${{VAR}}; shell command substitution is forbidden"
        );
    }
    Ok(())
}

fn is_var_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_var_continue(byte: u8) -> bool {
    is_var_start(byte) || byte.is_ascii_digit()
}

fn env_value(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| {
        format!("local path override references unset environment variable `{name}`")
    })
}

fn expand_env_with(
    raw: &str,
    lookup: impl Fn(&str) -> Result<String>,
) -> Result<String> {
    validate_raw("<path>", raw)?;
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            let ch = raw[index..]
                .chars()
                .next()
                .context("invalid UTF-8 character boundary")?;
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }

        if index + 1 >= bytes.len() {
            bail!("local path override ends with a bare `$`");
        }
        if bytes[index + 1] == b'{' {
            let name_start = index + 2;
            let Some(close_offset) = bytes[name_start..].iter().position(|byte| *byte == b'}')
            else {
                bail!("local path override has an unclosed `${{...}}` reference");
            };
            let close = name_start + close_offset;
            let name = &raw[name_start..close];
            let name_bytes = name.as_bytes();
            if name_bytes.is_empty()
                || !is_var_start(name_bytes[0])
                || !name_bytes.iter().skip(1).all(|byte| is_var_continue(*byte))
            {
                bail!("local path override has invalid environment variable name `{name}`");
            }
            out.push_str(&lookup(name)?);
            index = close + 1;
            continue;
        }

        let name_start = index + 1;
        if !is_var_start(bytes[name_start]) {
            bail!("local path override has unsupported shell syntax after `$`");
        }
        let mut end = name_start + 1;
        while end < bytes.len() && is_var_continue(bytes[end]) {
            end += 1;
        }
        let name = &raw[name_start..end];
        out.push_str(&lookup(name)?);
        index = end;
    }
    if out.chars().any(char::is_control) {
        bail!("expanded local path override contains control characters");
    }
    Ok(out)
}

/// Expand only $VAR and ${VAR}. This is deliberately not shell expansion:
/// no command substitution, tilde expansion, globbing, or word splitting.
pub(crate) fn expand_env(raw: &str) -> Result<String> {
    expand_env_with(raw, env_value)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

pub(crate) fn resolve(
    project: &Path,
    modules_dir: &str,
    raw: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, PathBuf>> {
    let canonical_project = fs::canonicalize(project)
        .with_context(|| format!("canonicalizing project {}", project.display()))?;
    let modules_path = canonical_project.join(modules_dir);
    let modules = fs::canonicalize(&modules_path).unwrap_or(modules_path);
    let staging = canonical_project.join(crate::transaction::STAGING_DIR);
    let mut resolved = BTreeMap::new();

    for (package, configured) in raw {
        let expanded = expand_env(configured)
            .with_context(|| format!("expanding local path override for `{package}`"))?;
        let candidate = PathBuf::from(expanded);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            canonical_project.join(candidate)
        };
        let canonical = fs::canonicalize(&candidate).with_context(|| {
            format!(
                "local path override for `{package}` does not resolve to an existing path: {}",
                candidate.display()
            )
        })?;
        if !canonical.is_dir() {
            bail!(
                "local path override for `{package}` is not a directory: {}",
                canonical.display()
            );
        }
        if paths_overlap(&canonical, &modules) {
            bail!(
                "local path override for `{package}` overlaps Zed package install directory {}",
                modules.display()
            );
        }
        if canonical.starts_with(&staging) {
            bail!(
                "local path override for `{package}` points into Zed transaction staging {}",
                staging.display()
            );
        }
        let manifest = canonical.join(MANIFEST_FILE);
        let metadata = fs::symlink_metadata(&manifest).with_context(|| {
            format!(
                "local path override for `{package}` has no {}",
                manifest.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            bail!("local path override for `{package}` must contain a regular {MANIFEST_FILE}");
        }
        resolved.insert(package.clone(), canonical);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, bail};

    use super::expand_env_with;

    fn test_lookup(name: &str) -> Result<String> {
        match name {
            "ZED_OVERRIDE_ROOT" => Ok("/tmp/zed-root".to_string()),
            other => bail!("unexpected variable {other}"),
        }
    }

    #[test]
    fn expands_only_named_environment_variables() {
        assert_eq!(
            expand_env_with("${ZED_OVERRIDE_ROOT}/pkg", test_lookup).unwrap(),
            "/tmp/zed-root/pkg"
        );
        assert_eq!(
            expand_env_with("$ZED_OVERRIDE_ROOT/pkg", test_lookup).unwrap(),
            "/tmp/zed-root/pkg"
        );
    }

    #[test]
    fn rejects_shell_syntax() {
        for value in [
            "$(touch /tmp/owned)",
            "`touch /tmp/owned`",
            "$",
            "${HOME",
            "$9BAD/path",
        ] {
            assert!(expand_env_with(value, test_lookup).is_err(), "{value}");
        }
    }
}
