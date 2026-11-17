use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use zed_cli::add_context::select;

mod tests {
    use super::*;

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

    // Windows canonicalization expands short names and uses the extended
    // \\?\\ path prefix. Compare canonical paths, never lexical spellings.
    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().unwrap()
    }

    #[test]
    fn zpkg_and_package_json_coexist_without_competing_as_roots() {
        let dir = tempfile::tempdir().unwrap();
        make(dir.path(), &[".zpkg.toml", "package.json", "src/main.ts"]);
        let selection = select(dir.path(), None, None).unwrap();
        assert_eq!(selection.root, canonical(dir.path()));
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
        assert_eq!(selection.root, canonical(dir.path()));
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
        assert_eq!(selected.root, canonical(&dir.path().join("apps")));
        assert_eq!(selected.target.as_deref(), Some("node"));

        // An explicit selection is exact, including when a nearer Zed
        // manifest exists. It never silently walks to an alternative root.
        let parent = select(&cwd, Some(dir.path()), Some("rust")).unwrap();
        assert_eq!(parent.root, canonical(dir.path()));
        assert_eq!(parent.target.as_deref(), Some("rust"));

        // From the directory containing the nearest manifest, it remains
        // the selected root even with another Zed manifest above it.
        let at_manifest = select(&dir.path().join("apps"), None, None).unwrap();
        assert_eq!(at_manifest.root, canonical(&dir.path().join("apps")));
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
        assert_eq!(selected.root, canonical(&dir.path().join("apps/api")));
        assert_eq!(selected.target.as_deref(), Some("attolang"));
        let parent = select(dir.path(), None, None).unwrap();
        assert_eq!(parent.root, canonical(dir.path()));
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
        assert_eq!(selected.root, canonical(dir.path()));
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
        let failure = select(dir.path(), None, Some("rust"))
            .unwrap_err()
            .to_string();
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
