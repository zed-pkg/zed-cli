//! Universal, read-only project/target selection for `zed add`.
//!
//! `.zpkg.toml` owns Zed dependency intent; a native manifest is only a
//! language context hint. No native manager or compiler is invoked here.
//! Ambiguous targets must be selected explicitly before registry I/O.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::language::Language;
use zed_interfaces::paths::MANIFEST_FILE;

use crate::config;
use crate::ops;

#[derive(Debug, PartialEq, Eq)]
pub struct AddSelection {
    pub root: PathBuf,
    /// Target for this Zed consumer manifest, not a compiler invocation.
    pub target: Option<String>,
}

/// Choose the nearest Zed package authority, then the most local *unique*
/// native consumer target. An explicit --project always names an exact Zed
/// root; it is not expanded into another ancestor's dependency graph.
pub fn select(
    cwd: &Path,
    project: Option<&Path>,
    explicit_target: Option<&str>,
) -> Result<AddSelection> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("invalid current directory {}", cwd.display()))?;
    let root = match project {
        Some(project) => {
            let candidate = if project.is_absolute() {
                project.to_path_buf()
            } else {
                cwd.join(project)
            };
            candidate.canonicalize().with_context(|| format!("cannot resolve --project {}", candidate.display()))?
        }
        None => cwd.ancestors()
            .find(|ancestor| ancestor.join(MANIFEST_FILE).is_file())
            .map(Path::to_path_buf)
            .with_context(|| format!(
                "no {MANIFEST_FILE} found at or above {}; use --project PATH or zed init / zed install PACKAGE first",
                cwd.display()
            ))?,
    };
    if !root.is_dir() || !root.join(MANIFEST_FILE).is_file() {
        bail!(
            "selected Zed project {} has no {MANIFEST_FILE}; use zed init or zed install PACKAGE",
            root.display()
        );
    }

    // An explicit --target must never launder dual native package markers.
    let mut directory = Some(if cwd.starts_with(&root) {
        cwd.as_path()
    } else {
        root.as_path()
    });
    while let Some(path) = directory {
        native_targets(path)?;
        if path == root {
            break;
        }
        directory = path.parent();
    }

    let manifest = config::read_manifest(&root)?;
    let configured = manifest.requested_target();
    let explicit = explicit_target.map(str::trim);
    if explicit.is_some_and(|value| {
        value.is_empty()
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    }) {
        bail!("--target must be a nonempty language/target token (e.g. attolang, nodejs, rust)");
    }
    if let (Some(explicit), Some(configured)) = (explicit, configured)
        && !same_target(explicit, configured)
    {
        bail!(
            "this Zed project already selects target `{configured}`; cannot add for `{explicit}`.              Use a separate member .zpkg.toml or deliberately update [install].target"
        );
    }

    let selected = explicit.or(configured);
    if let Some(selected) = selected {
        // A durable consumer target is authoritative, but a caller who walks
        // into a different native project should not silently redirect that
        // project's dependency to the parent's language. A deliberate
        // --target/--project remains the explicit override.
        if explicit.is_none() && cwd.starts_with(&root) && cwd != root {
            let mut directory = Some(cwd.as_path());
            while let Some(path) = directory {
                if path == root {
                    break;
                }
                let found = native_targets(path)?;
                if !found.is_empty() {
                    if !found
                        .iter()
                        .any(|candidate| same_target(candidate, selected))
                    {
                        bail!(
                            "native project {} targets {}, but Zed consumer {} selects {}; create a member .zpkg.toml or choose --project explicitly",
                            path.display(),
                            found.into_iter().collect::<Vec<_>>().join(", "),
                            root.display(),
                            selected
                        );
                    }
                    break;
                }
                directory = path.parent();
            }
        }
        return Ok(AddSelection {
            root,
            target: Some(selected.to_owned()),
        });
    }
    let consumer = if cwd.starts_with(&root) {
        cwd.as_path()
    } else {
        root.as_path()
    };
    let mut ancestor = Some(consumer);
    while let Some(directory) = ancestor {
        let detected = native_targets(directory)?;
        if !detected.is_empty() {
            return Ok(AddSelection {
                root,
                target: Some(unique_target(directory, detected)?),
            });
        }
        if directory == root {
            break;
        }
        ancestor = directory.parent();
    }

    // When invoked at the package root, discover only nearby native member
    // contexts. Never traverse another Zed consumer project, the store, a
    // symlink, or an unbounded directory graph.
    let found = nested_targets(&root)?;
    Ok(AddSelection {
        target: if found.is_empty() {
            None
        } else {
            Some(unique_target(&root, found)?)
        },
        root,
    })
}

fn same_target(left: &str, right: &str) -> bool {
    match (Language::from_token(left), Language::from_token(right)) {
        (Some(a), Some(b)) => a == b,
        _ => {
            left == right
                || matches!(
                    (left, right),
                    ("attolang", "oreslang") | ("oreslang", "attolang")
                )
        }
    }
}

fn native_targets(directory: &Path) -> Result<BTreeSet<String>> {
    if directory.join(".atto.pkg.toml").exists() && directory.join(".ores.pkg.toml").exists() {
        bail!(
            "ambiguous AttoLang compiler markers in {}: migrate .ores.pkg.toml to .atto.pkg.toml; do not keep both",
            directory.display()
        );
    }
    Ok(ops::native_manifest_targets(directory))
}

fn unique_target(directory: &Path, targets: BTreeSet<String>) -> Result<String> {
    if targets.len() != 1 {
        bail!(
            "ambiguous language context in {} ({}). Select --target LANG and optionally --project PATH; Zed will not guess or edit native manifests",
            directory.display(),
            targets.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(targets.into_iter().next().expect("one language"))
}

fn nested_targets(root: &Path) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > 0 {
            if directory.join(MANIFEST_FILE).is_file() {
                continue; // Separate Zed authority: never infer across it.
            }
            found.extend(native_targets(&directory)?);
        }
        if depth >= 2 {
            continue;
        }
        for child in fs::read_dir(&directory)
            .with_context(|| format!("inspect Zed project {}", directory.display()))?
        {
            let child = child?;
            let file_type = child.file_type()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = child.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "node_modules"
                        | "zed_modules"
                        | "target"
                        | "vendor"
                        | "dist"
                        | "build"
                        | "out"
                        | ".git"
                )
            {
                continue;
            }
            pending.push((child.path(), depth + 1));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make(root: &Path, paths: &[&str]) {
        for file in paths {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, if *file == ".zpkg.toml" { "[package]\norg='a'\nname='b'\nversion='0.1.0'\n[package.repository]\nvcs='git'\nurl='https://example.invalid/a/b'\n" } else { "" }).unwrap();
        }
    }

    #[test]
    fn zpkg_and_package_json_coexist_without_competing_as_roots() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml", "package.json", "src/main.ts"]);
        let selection = select(dir.path(), None, None).unwrap();
        assert_eq!(selection.root, dir.path());
        assert_eq!(selection.target.as_deref(), Some("node"));
    }

    #[test]
    fn canonical_atto_and_legacy_alias_are_one_target() {
        for marker in [".atto.pkg.toml", ".ores.pkg.toml"] {
            let dir = tempfile::tempdir().unwrap();
            make(dir.path(), &[".zpkg.toml", marker]);
            assert_eq!(
                select(dir.path(), None, None).unwrap().target.as_deref(),
                Some("attolang")
            );
        }
    }

    #[test]
    fn mixed_native_manifests_fail_without_explicit_target() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[".zpkg.toml", "package.json", ".atto.pkg.toml"],
        );
        let failure = select(dir.path(), None, None).unwrap_err().to_string();
        assert!(failure.contains("ambiguous language context"), "{failure}");
        assert_eq!(
            select(dir.path(), None, Some("attolang"))
                .unwrap()
                .target
                .as_deref(),
            Some("attolang")
        );
    }

    #[test]
    fn nearest_native_project_wins_below_one_zpkg_authority() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[".zpkg.toml", "package.json", "apps/api/.atto.pkg.toml"],
        );
        let from_api = dir.path().join("apps/api/src");
        fs::create_dir_all(&from_api).unwrap();
        let selection = select(&from_api, None, None).unwrap();
        assert_eq!(selection.root, dir.path());
        assert_eq!(selection.target.as_deref(), Some("attolang"));
    }

    #[test]
    fn sibling_mixed_consumers_require_choice_at_root() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[
                ".zpkg.toml",
                "apps/api/.atto.pkg.toml",
                "apps/web/package.json",
            ],
        );
        let message = select(dir.path(), None, None).unwrap_err().to_string();
        assert!(message.contains("ambiguous language context"), "{message}");
    }

    #[test]
    fn nested_zpkg_project_owns_its_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[
                ".zpkg.toml",
                "apps/api/.zpkg.toml",
                "apps/api/.atto.pkg.toml",
            ],
        );
        let from_api = dir.path().join("apps/api/src");
        fs::create_dir_all(&from_api).unwrap();
        let selected = select(&from_api, None, None).unwrap();
        assert_eq!(selected.root, dir.path().join("apps/api"));
        assert_eq!(selected.target.as_deref(), Some("attolang"));
        let parent = select(dir.path(), None, None).unwrap();
        assert_eq!(parent.root, dir.path());
        assert!(parent.target.is_none());
    }

    #[test]
    fn explicit_project_is_exact_not_parent_inference() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml", "apps/api/.atto.pkg.toml"]);
        let err = select(dir.path(), Some(Path::new("apps/api")), None).unwrap_err();
        assert!(err.to_string().contains("no .zpkg.toml"));
    }

    #[test]
    fn never_use_first_manifest_arbitrarily_in_mixed_project() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[".zpkg.toml", "package.json", "Cargo.toml", ".atto.pkg.toml"],
        );
        assert!(select(dir.path(), None, None).is_err());
        assert!(select(dir.path(), None, Some("rust")).is_ok());
    }

    #[test]
    fn dual_atto_markers_are_migration_conflict() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[".zpkg.toml", ".atto.pkg.toml", ".ores.pkg.toml"],
        );
        assert!(
            select(dir.path(), None, None)
                .unwrap_err()
                .to_string()
                .contains("migrate")
        );
        assert!(
            select(dir.path(), None, Some("attolang"))
                .unwrap_err()
                .to_string()
                .contains("migrate")
        );
    }

    #[test]
    fn configured_target_cannot_silently_overrule_a_nested_other_language() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml", "apps/api/.atto.pkg.toml"]);
        let manifest = dir.path().join(".zpkg.toml");
        fs::OpenOptions::new()
            .append(true)
            .open(&manifest)
            .unwrap()
            .write_all(b"\n[install]\ntarget='node'\n")
            .unwrap();
        let nested = dir.path().join("apps/api/src");
        fs::create_dir_all(&nested).unwrap();
        let message = select(&nested, None, None).unwrap_err().to_string();
        assert!(message.contains("create a member .zpkg.toml"), "{message}");
        assert_eq!(
            select(&nested, None, Some("node"))
                .unwrap()
                .target
                .as_deref(),
            Some("node")
        );
    }

    #[test]
    fn pure_zed_project_can_remain_universal() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml"]);
        assert!(select(dir.path(), None, None).unwrap().target.is_none());
    }

    #[test]
    fn unknown_target_tokens_are_not_path_inputs() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml"]);
        for target in ["../outside", "node;echo", ""] {
            assert!(select(dir.path(), None, Some(target)).is_err());
        }
    }
}
