//! Read-only bridge from Zed's installed package index to AttoLang imports.
//!
//! AttoLang (formerly Oreslang) currently reads colon-separated ORES_PATHS.
//! Zed is the artifact/lock authority; this module only exports paths that
//! Zed already materialized, without running a compiler or modifying state.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

// This file is a local install index, not a bulk data transport. Bound the
// read before parsing so malformed or tampered indexes cannot exhaust memory.
const MAX_PATHS_INDEX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct PathsIndex {
    modules_dir: String,
    packages: Vec<IndexedPackage>,
}

#[derive(Debug, Deserialize)]
struct IndexedPackage {
    package: String,
    version: String,
    #[serde(default)]
    language: String,
    path: String,
}

/// Return deterministic, invocation-directory-relative import roots suitable
/// for joining with ":" in ORES_PATHS. Nothing is written or executed.
///
/// Until the shared language manifest adds canonical attolang tokens,
/// packages are identified by their native .ores.pkg.toml marker.
/// The old oreslang and future attolang index tags are also accepted.
pub fn installed_paths(cwd: &Path) -> Result<Vec<String>> {
    let root = find_project_root(cwd)?;
    let index_path = root.join(".zed").join("paths.json");
    let file = fs::File::open(&index_path).with_context(|| {
        format!(
            "no installed Zed package index at {}; run zed install --frozen after creating .zpkg.lock",
            index_path.display()
        )
    })?;
    let mut contents = String::new();
    file.take(MAX_PATHS_INDEX_BYTES + 1)
        .read_to_string(&mut contents)
        .with_context(|| format!("failed to read Zed package index {}", index_path.display()))?;
    if contents.len() as u64 > MAX_PATHS_INDEX_BYTES {
        bail!(
            "Zed package index {} exceeds the {} byte limit; refusing import paths",
            index_path.display(),
            MAX_PATHS_INDEX_BYTES
        );
    }
    let index: PathsIndex = serde_json::from_str(&contents)
        .with_context(|| format!("invalid Zed package index {}", index_path.display()))?;
    let modules = checked_relative(&index.modules_dir)?;
    let ascent = cwd
        .strip_prefix(&root)
        .context("AttoLang project root is not an ancestor of the current directory")?
        .components()
        .count();
    let mut from_cwd = PathBuf::new();
    for _ in 0..ascent {
        from_cwd.push("..");
    }

    let mut selected = BTreeSet::new();
    let mut seen_packages = BTreeSet::new();
    for package in index.packages {
        let (org, name) = package
            .package
            .split_once('/')
            .filter(|(org, name)| valid_segment(org) && valid_segment(name))
            .with_context(|| format!("invalid indexed package name {:?}", package.package))?;
        if package.version.is_empty() {
            bail!("indexed package {}/{} has an empty version", org, name);
        }
        // There can be exactly one immutable resolved package per org/name
        // slot. Do not silently deduplicate a poisoned index: duplicate
        // identities can hide conflicting versions or mismatched metadata.
        if !seen_packages.insert(package.package.clone()) {
            bail!(
                "duplicate indexed package {}/{}; refusing ambiguous import paths (run zed install --frozen)",
                org,
                name
            );
        }
        let relative = checked_relative(&package.path)?;
        let expected = modules.join(org).join(name);
        if relative != expected {
            bail!(
                "indexed package {}/{} has a path outside its module slot: {}",
                org,
                name,
                package.path
            );
        }
        let absolute = root.join(&relative);
        let is_atto = matches!(package.language.as_str(), "attolang" | "oreslang")
            || absolute.join(".ores.pkg.toml").is_file();
        if !is_atto {
            continue;
        }
        if !absolute.is_dir() {
            bail!(
                "AttoLang package {}/{} is not installed at {}; run zed install --frozen",
                org,
                name,
                absolute.display()
            );
        }
        let path = from_cwd.join(&relative);
        selected.insert(path.to_string_lossy().replace('\\', "/"));
    }
    if selected.is_empty() {
        bail!(
            "no installed AttoLang packages found in {}; packages need .ores.pkg.toml until the shared attolang language contract lands",
            index_path.display()
        );
    }
    Ok(selected.into_iter().collect())
}

fn find_project_root(cwd: &Path) -> Result<PathBuf> {
    for ancestor in cwd.ancestors() {
        if ancestor.join(".ores.pkg.toml").is_file()
            || ancestor.join(".zpkg.toml").is_file()
            || ancestor.join(".zed").join("paths.json").is_file()
        {
            return Ok(ancestor.to_path_buf());
        }
    }
    bail!(
        "no AttoLang/Zed project root found above {}; expected .ores.pkg.toml or .zpkg.toml",
        cwd.display()
    )
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Prevent path-list delimiters, traversal, and absolute targets from entering
/// compiler search paths. Zed's symlink or copy store mode is still supported:
/// these are lexical paths inside the project's installed module slots.
fn checked_relative(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() || raw.chars().any(|c| matches!(c, ':' | '\n' | '\r' | '\0')) {
        bail!("invalid package index path (empty or unsafe ORES_PATHS character)");
    }
    let path = Path::new(raw);
    if !path
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        bail!("package index path must be relative and traversal-free: {raw}");
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{checked_relative, valid_segment};

    #[test]
    fn rejects_import_path_injection() {
        for value in [
            "",
            "../outside",
            "/tmp/rogue",
            "zed_modules/../rogue",
            "a:b",
            "a\nb",
            ".",
            "x\0y",
        ] {
            assert!(checked_relative(value).is_err(), "{value:?}");
        }
        assert!(checked_relative("zed_modules/acme/actor").is_ok());
    }

    #[test]
    fn restricts_package_slot_names() {
        for value in ["", "..", "evil/name", "UPPER", "-leading", "trailing-"] {
            assert!(!valid_segment(value), "{value:?}");
        }
        assert!(valid_segment("acme-utils-2"));
    }
}
