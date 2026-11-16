//! Universal, read-only project/target selection for `zed add`.
//!
//! `.zpkg.toml` owns Zed dependency intent; a native manifest is only a
//! language context hint. No native manager or compiler is invoked here.
//! Ambiguous targets must be selected explicitly before registry I/O.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
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
            candidate
                .canonicalize()
                .with_context(|| format!("cannot resolve --project {}", candidate.display()))?
        }
        None => {
            let mut nearest = None;
            for ancestor in cwd.ancestors() {
                if regular_marker(ancestor, MANIFEST_FILE)? {
                    nearest = Some(ancestor.to_path_buf());
                    break;
                }
            }
            nearest.with_context(|| format!(
                "no {MANIFEST_FILE} found at or above {}; use --project PATH or zed init / zed install PACKAGE first",
                cwd.display()
            ))?
        }
    };
    if !root.is_dir() || !regular_marker(&root, MANIFEST_FILE)? {
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
    // A CLI target is authoritative, but `zed add` writes a durable
    // dependency declaration. Changing the consumer-wide default while
    // existing direct requirements remain would reinterpret them on the
    // next frozen install. Allow an explicit switch only for an empty
    // consumer; otherwise use separate target-specific Zed manifests.
    if let Some(explicit) = explicit
        && !configured.is_some_and(|configured| same_target(explicit, configured))
        && (!manifest.dependencies.is_empty() || !manifest.build_dependencies.is_empty())
    {
        bail!(
            "cannot override configured target `{}` with `{explicit}` in {}: this Zed consumer already has dependencies. Use a separate member .zpkg.toml for independently targeted packages",
            configured.unwrap_or("<unspecified>"),
            root.display()
        );
    }

    // Preserve the current spelling for equivalent aliases such as node /
    // nodejs. The explicit override can change the durable target only
    // when no existing direct dependency would be reinterpreted.
    let selected = match (explicit, configured) {
        (Some(explicit), Some(configured)) if same_target(explicit, configured) => Some(configured),
        (Some(explicit), _) => Some(explicit),
        (None, configured) => configured,
    };
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
            let target = unique_target(directory, detected)?;
            return Ok(AddSelection {
                root,
                target: Some(target),
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

/// A manifest is trusted as a project selector only when its directory
/// contains a *regular file*. A symlink (including a dangling one) must not
/// silently redirect Zed into a different package authority, and a directory
/// named like a manifest is not a valid project marker.
fn regular_marker(directory: &Path, name: &str) -> Result<bool> {
    let path = directory.join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if !metadata.file_type().is_file() {
        bail!(
            "unsafe project marker {}: expected a regular file, not a symlink or directory",
            path.display()
        );
    }
    Ok(true)
}

fn native_targets(directory: &Path) -> Result<BTreeSet<String>> {
    // Native AttoLang markers are compiler-owned, but project inference must
    // not follow a symlink to an unrelated outside source tree.
    let canonical = regular_marker(directory, ".atto.pkg.toml")?;
    let legacy = regular_marker(directory, ".ores.pkg.toml")?;
    if canonical && legacy {
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
    const MAX_SCANNED_ENTRIES: usize = 4096;
    let mut scanned = 0usize;
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > 0 {
            if regular_marker(&directory, MANIFEST_FILE)? {
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
            scanned += 1;
            if scanned > MAX_SCANNED_ENTRIES {
                bail!(
                    "target inference scanned more than {MAX_SCANNED_ENTRIES} directory entries under {}; set [install].target in .zpkg.toml or pass --target",
                    root.display()
                );
            }
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
            let content = if path.file_name().and_then(|name| name.to_str()) == Some(".zpkg.toml") {
                r#"[package]
org='a'
name='b'
version='0.1.0'
[package.repository]
vcs='git'
url='https://example.invalid/a/b'
"#
            } else {
                ""
            };
            fs::write(path, content).unwrap();
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
    fn project_defaults_to_nearest_zpkg_ancestor_not_the_repository_root() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[
                ".zpkg.toml",
                "apps/.zpkg.toml",
                "apps/api/package.json",
                "apps/api/src/main.ts",
            ],
        );
        let cwd = dir.path().join("apps/api/src");
        let selected = select(&cwd, None, None).unwrap();
        assert_eq!(selected.root, dir.path().join("apps"));
        assert_eq!(selected.target.as_deref(), Some("node"));

        // An explicit selection is exact, including when a nearer Zed
        // manifest exists. It never silently walks to an alternative root.
        let parent = select(&cwd, Some(dir.path()), Some("rust")).unwrap();
        assert_eq!(parent.root, dir.path());
        assert_eq!(parent.target.as_deref(), Some("rust"));

        // From the directory containing the nearest manifest, it remains
        // the selected root even with another Zed manifest above it.
        let at_manifest = select(&dir.path().join("apps"), None, None).unwrap();
        assert_eq!(at_manifest.root, dir.path().join("apps"));
    }

    #[test]
    fn no_ancestor_zpkg_has_actionable_error_not_silent_creation() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &["apps/api/package.json"]);
        let cwd = dir.path().join("apps/api");
        let result = select(&cwd, None, None).unwrap_err().to_string();
        assert!(result.contains("no .zpkg.toml found"), "{result}");
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
    fn zpkg_default_overrides_mixed_native_markers_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[".zpkg.toml", ".atto.pkg.toml", "package.json"],
        );
        let manifest = dir.path().join(".zpkg.toml");
        fs::OpenOptions::new()
            .append(true)
            .open(&manifest)
            .unwrap()
            .write_all(b"\n[install]\ntarget='attolang'\n")
            .unwrap();
        let selected = select(dir.path(), None, None).unwrap();
        assert_eq!(selected.root, dir.path());
        assert_eq!(selected.target.as_deref(), Some("attolang"));
    }

    #[test]
    fn explicit_target_can_override_empty_zpkg_consumer() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[".zpkg.toml", ".atto.pkg.toml", "package.json"],
        );
        let manifest = dir.path().join(".zpkg.toml");
        fs::OpenOptions::new()
            .append(true)
            .open(&manifest)
            .unwrap()
            .write_all(b"\n[install]\ntarget='attolang'\n")
            .unwrap();
        let selection = select(dir.path(), None, Some("nodejs")).unwrap();
        assert_eq!(selection.target.as_deref(), Some("nodejs"));
        assert_eq!(
            select(dir.path(), None, None).unwrap().target.as_deref(),
            Some("attolang")
        );
    }

    #[test]
    fn explicit_target_cannot_reinterpret_existing_dependency_set() {
        let dir = tempfile::tempdir().unwrap();
        make(
            dir.path(),
            &[".zpkg.toml", ".atto.pkg.toml", "package.json"],
        );
        let manifest = dir.path().join(".zpkg.toml");
        fs::OpenOptions::new()
            .append(true)
            .open(&manifest)
            .unwrap()
            .write_all(b"\n[dependencies]\n'acme/http'='^1.0'\n[install]\ntarget='attolang'\n")
            .unwrap();
        let err = select(dir.path(), None, Some("nodejs")).unwrap_err();
        assert!(
            err.to_string().contains("already has dependencies"),
            "{err}"
        );
        assert_eq!(
            select(dir.path(), None, None).unwrap().target.as_deref(),
            Some("attolang")
        );
    }

    #[test]
    fn unscoped_dependencies_cannot_be_reinterpreted_by_new_target() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml", "package.json"]);
        let manifest = dir.path().join(".zpkg.toml");
        fs::OpenOptions::new()
            .append(true)
            .open(&manifest)
            .unwrap()
            .write_all(b"\n[dependencies]\n'acme/core'='^1'\n")
            .unwrap();
        let failure = select(dir.path(), None, Some("rust")).unwrap_err().to_string();
        assert!(failure.contains("already has dependencies"), "{failure}");
        assert!(failure.contains("<unspecified>"), "{failure}");
    }

    #[test]
    fn equivalent_target_alias_does_not_rewrite_the_default() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml", "package.json"]);
        let manifest = dir.path().join(".zpkg.toml");
        fs::OpenOptions::new()
            .append(true)
            .open(&manifest)
            .unwrap()
            .write_all(b"\n[install]\ntarget='node'\n")
            .unwrap();
        let selection = select(dir.path(), None, Some("nodejs")).unwrap();
        assert_eq!(selection.target.as_deref(), Some("node"));
    }

    #[test]
    fn pure_zed_project_can_remain_universal() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml"]);
        assert!(select(dir.path(), None, None).unwrap().target.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_manifest_cannot_be_selected_as_project() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        make(root.path(), &[".zpkg.toml"]);
        let member = root.path().join("nested");
        fs::create_dir_all(&member).unwrap();
        symlink(root.path().join(".zpkg.toml"), member.join(".zpkg.toml")).unwrap();
        let fail = select(&member, None, None).unwrap_err().to_string();
        assert!(fail.contains("unsafe project marker"), "{fail}");
        let explicit = select(root.path(), Some(&member), None)
            .unwrap_err()
            .to_string();
        assert!(explicit.contains("unsafe project marker"), "{explicit}");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_zpkg_marker_cannot_redirect_to_parent_project() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        make(root.path(), &[".zpkg.toml"]);
        let member = root.path().join("nested");
        fs::create_dir_all(&member).unwrap();
        symlink("missing.toml", member.join(".zpkg.toml")).unwrap();
        let fail = select(&member, None, None).unwrap_err().to_string();
        assert!(fail.contains("unsafe project marker"), "{fail}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_atto_source_identity_is_not_a_language_hint() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        make(root.path(), &[".zpkg.toml"]);
        let external = root.path().join("external.toml");
        fs::write(&external, "[project]\\nname='outside'\\n").unwrap();
        symlink(&external, root.path().join(".atto.pkg.toml")).unwrap();
        let fail = select(root.path(), None, Some("attolang"))
            .unwrap_err()
            .to_string();
        assert!(fail.contains("unsafe project marker"), "{fail}");
    }

    #[test]
    fn bounded_native_inference_rejects_unreasonably_wide_workspaces() {
        let root = tempfile::tempdir().unwrap();
        make(root.path(), &[".zpkg.toml"]);
        for i in 0..4097 {
            fs::create_dir(root.path().join(format!("member_{i:05}"))).unwrap();
        }
        let fail = select(root.path(), None, None).unwrap_err().to_string();
        assert!(
            fail.contains("target inference scanned more than"),
            "{fail}"
        );
        assert!(fail.contains("--target"), "{fail}");
    }

    #[test]
    fn configured_target_bypasses_unnecessary_workspace_scan() {
        let root = tempfile::tempdir().unwrap();
        make(root.path(), &[".zpkg.toml"]);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(root.path().join(".zpkg.toml"))
            .unwrap();
        file.write_all(b"\n[install]\ntarget='attolang'\n").unwrap();
        for i in 0..4097 {
            fs::create_dir(root.path().join(format!("member_{i:05}"))).unwrap();
        }
        let selected = select(root.path(), None, None).unwrap();
        assert_eq!(selected.target.as_deref(), Some("attolang"));
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
