use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::MANIFEST_FILE;

use crate::config::read_manifest;

/// Expand only portable environment references in an authored local path.
///
/// This is intentionally *not* shell expansion: command substitution, tilde
/// expansion, globbing, and shell arithmetic are outside the dependency
/// contract. The manifest validator rejects those forms too, but the runtime
/// parser remains fail-closed when called independently.
pub(crate) fn expand_env_path(template: &str) -> Result<String> {
    expand_env_path_with(template, |name| {
        env::var(name).with_context(|| {
            format!("local dependency path references unset or non-UTF-8 environment variable `{name}`")
        })
    })
}

fn expand_env_path_with(
    template: &str,
    mut lookup: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    if template.contains('`') || template.contains("$(") {
        bail!("local dependency paths allow $NAME or ${{NAME}}, not shell command substitution");
    }

    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != '$' {
            out.push(chars[index]);
            index += 1;
            continue;
        }

        index += 1;
        if index == chars.len() {
            bail!("trailing `$` is not a valid environment reference");
        }

        let name = if chars[index] == '{' {
            index += 1;
            let start = index;
            while index < chars.len() && chars[index] != '}' {
                index += 1;
            }
            if index == chars.len() {
                bail!("unterminated `${{NAME}}` environment reference");
            }
            let name: String = chars[start..index].iter().collect();
            index += 1;
            name
        } else {
            let start = index;
            while index < chars.len()
                && (chars[index] == '_' || chars[index].is_ascii_alphanumeric())
            {
                index += 1;
            }
            chars[start..index].iter().collect()
        };

        validate_env_name(&name)?;
        out.push_str(&lookup(&name)?);
    }
    Ok(out)
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    if !valid {
        bail!("invalid environment variable name `{name}` in local dependency path");
    }
    Ok(())
}

fn lexical_absolute(project: &Path, expanded: &str) -> PathBuf {
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        project.join(path)
    }
}

fn path_is_within(candidate: &Path, root: &Path) -> bool {
    candidate == root || candidate.starts_with(root)
}

/// Resolve, canonicalize, and identity-check every explicit local dependency
/// override. The manifest remains keyed by canonical `org/name`; a path may
/// not silently provide a differently named package.
pub(crate) fn resolve(
    project: &Path,
    manifest: &Manifest,
) -> Result<BTreeMap<String, PathBuf>> {
    if manifest.overrides.path.is_empty() {
        return Ok(BTreeMap::new());
    }

    let canonical_project = project
        .canonicalize()
        .with_context(|| format!("canonicalizing project {}", project.display()))?;
    let materialized_root = canonical_project.join(manifest.modules_dir());
    let mut resolved = BTreeMap::new();

    for (key, template) in &manifest.overrides.path {
        let expanded = expand_env_path(template)
            .with_context(|| format!("expanding local override for `{key}`"))?;
        let candidate = lexical_absolute(&canonical_project, &expanded);
        let canonical = candidate.canonicalize().with_context(|| {
            format!(
                "local override for `{key}` points at missing or inaccessible path {}",
                candidate.display()
            )
        })?;
        if !canonical.is_dir() {
            bail!(
                "local override for `{key}` must be a directory, got {}",
                canonical.display()
            );
        }
        if path_is_within(&canonical, &materialized_root) {
            bail!(
                "local override for `{key}` resolves inside the Zed materialization tree {}; use an independent source checkout",
                materialized_root.display()
            );
        }

        let dep_manifest = read_manifest(&canonical).with_context(|| {
            format!(
                "local override for `{key}` must contain a valid {MANIFEST_FILE} at {}",
                canonical.display()
            )
        })?;
        let actual = dep_manifest.full_name();
        if actual != *key {
            bail!(
                "local override for `{key}` provides package `{actual}` at {}",
                canonical.display()
            );
        }
        resolved.insert(key.clone(), canonical);
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_braced_and_unbraced_environment_references() {
        let expanded = expand_env_path_with("${HOME}/codes/$ORG/pkg", |name| match name {
            "HOME" => Ok("/Users/alex".to_string()),
            "ORG" => Ok("acme".to_string()),
            other => bail!("unexpected variable {other}"),
        })
        .unwrap();
        assert_eq!(expanded, "/Users/alex/codes/acme/pkg");
    }

    #[test]
    fn rejects_shell_substitution_and_malformed_variables() {
        for value in ["$(id)", "`id`", "$", "${BAD-NAME}/pkg"] {
            assert!(
                expand_env_path_with(value, |_| Ok("ignored".to_string())).is_err(),
                "{value} must fail"
            );
        }
    }

    #[test]
    fn missing_environment_variable_is_an_error() {
        let error = expand_env_path_with("$MISSING/pkg", |name| bail!("missing {name}"))
            .unwrap_err();
        assert!(error.to_string().contains("missing MISSING"));
    }
}
