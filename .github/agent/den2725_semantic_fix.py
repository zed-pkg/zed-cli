#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement target, found {count}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/bin/zed_gitops/10_cli.rs",
    "fn run_validate(args: ValidateArgs) -> Result<i32> {\n    let report = validate_catalog(&args.root, &args.catalog, args.strict, args.offline)?;",
    "fn run_validate(args: ValidateArgs) -> Result<i32> {\n    if !args.offline {\n        bail!(\"online validation is not implemented; pass --offline\");\n    }\n    let report = validate_catalog(&args.root, &args.catalog, args.strict, true)?;",
)

replace_once(
    "src/bin/zed_gitops/10_cli.rs",
    "    if catalog_metadata.file_type().is_symlink() || !catalog_metadata.is_dir() {\n        bail!(\n            \"catalog {} must be a real directory inside the superproject\",\n            catalog.display()\n        );\n    }\n\n    let modules = configured_submodules(&root)?",
    "    if catalog_metadata.file_type().is_symlink() || !catalog_metadata.is_dir() {\n        bail!(\n            \"catalog {} must be a real directory inside the superproject\",\n            catalog.display()\n        );\n    }\n    let catalog = fs::canonicalize(&catalog)\n        .with_context(|| format!(\"canonicalizing catalog directory {}\", catalog.display()))?;\n    if !catalog.starts_with(&root) {\n        bail!(\n            \"catalog {} escapes the superproject root {}\",\n            catalog.display(),\n            root.display()\n        );\n    }\n\n    let modules = configured_submodules(&root)?",
)

replace_once(
    "src/bin/zed_gitops/20_validate.rs",
    "    if !is_exact_sha1(&inventory.revision) {",
    "    if !is_explicit_github_repository(&inventory.repository) {\n        diagnostics.push(Diagnostic::error(\n            \"inventory.repository\",\n            \"inventory repository must identify exactly one GitHub owner/repository\",\n            relative,\n            app,\n        ));\n    }\n    if !is_exact_sha1(&inventory.revision) {",
)

replace_once(
    "src/bin/zed_gitops/20_validate.rs",
    '''    } else {
        let static_path = root.join(&migration.static_application);
        match fs::symlink_metadata(&static_path) {
            Ok(metadata)
                if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => diagnostics.push(Diagnostic::error(
                "migration.static-application-missing",
                format!(
                    "static Application path is not a regular file: {}",
                    migration.static_application
                ),
                relative,
                app,
            )),
            Err(_) => diagnostics.push(Diagnostic::error(
                "migration.static-application-missing",
                format!(
                    "static Application path does not exist: {}",
                    migration.static_application
                ),
                relative,
                app,
            )),
        }
    }
''',
    '''    } else if !is_regular_file_within_root(root, &migration.static_application) {
        diagnostics.push(Diagnostic::error(
            "migration.static-application-missing",
            format!(
                "static Application path must be a regular file within the superproject: {}",
                migration.static_application
            ),
            relative,
            app,
        ));
    }
''',
)

replace_once(
    "src/bin/zed_gitops/40_git.rs",
    "fn validate_relative_path(value: &str) -> Result<()> {",
    '''fn is_explicit_github_repository(value: &str) -> bool {
    let normalized = normalize_repository_url(value);
    let Some(identity) = normalized.strip_prefix("github.com/") else {
        return false;
    };
    let mut parts = identity.split('/');
    matches!(parts.next(), Some(part) if !part.is_empty())
        && matches!(parts.next(), Some(part) if !part.is_empty())
        && parts.next().is_none()
}

fn is_regular_file_within_root(root: &Path, relative: &str) -> bool {
    if validate_relative_path(relative).is_err() {
        return false;
    }
    let candidate = root.join(relative);
    let Ok(metadata) = fs::symlink_metadata(&candidate) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    fs::canonicalize(candidate).is_ok_and(|resolved| resolved.starts_with(root))
}

fn validate_relative_path(value: &str) -> Result<()> {''',
)

replace_once(
    "docs/gitops-validator.md",
    "The command does not read Kubernetes credentials, clone private repositories,\nresolve remote branch tips, or apply manifests. Policy failures exit with code\n2; tool/configuration failures exit with code 1.",
    "The command does not read Kubernetes credentials, clone private repositories,\nresolve remote branch tips, or apply manifests. Online validation is not implemented\nyet, so invocations must pass `--offline`; omitting it fails explicitly instead of\nmisreporting a local-only run as online evidence. Policy failures exit with code 2;\ntool/configuration failures exit with code 1.",
)

tests = Path("src/bin/zed_gitops/90_tests.rs")
text = tests.read_text(encoding="utf-8")
old_import = "    use super::{\n        DEFAULT_CATALOG, normalize_repository_url, validate_catalog, validate_relative_path,\n    };"
new_import = "    use super::{\n        DEFAULT_CATALOG, OutputFormat, ValidateArgs, normalize_repository_url, run_validate,\n        validate_catalog, validate_relative_path,\n    };"
if text.count(old_import) != 1:
    raise SystemExit("test import target missing or duplicated")
text = text.replace(old_import, new_import, 1)
marker = "    #[test]\n    fn strict_mode_rejects_unknown_fields() {"
if text.count(marker) != 1:
    raise SystemExit("test insertion marker missing or duplicated")
additions = r'''    #[test]
    fn online_mode_fails_instead_of_claiming_unimplemented_evidence() {
        let (directory, _) = fixture();
        let error = run_validate(ValidateArgs {
            root: directory.path().to_path_buf(),
            catalog: PathBuf::from(DEFAULT_CATALOG),
            format: OutputFormat::Human,
            strict: true,
            offline: false,
        })
        .expect_err("online validation must fail until implemented");
        assert!(error.to_string().contains("pass --offline"));
    }

    #[test]
    fn non_github_inventory_repository_fails_closed() {
        let (directory, record_path) = fixture();
        let mut record: serde_json::Value = serde_json::from_slice(
            &fs::read(&record_path).expect("read record"),
        )
        .expect("parse record");
        record["spec"]["inventory"]["repository"] =
            serde_json::Value::String("../fabrication-server.rs".into());
        record["spec"]["source"]["repository"] =
            serde_json::Value::String("../fabrication-server.rs".into());
        fs::write(
            &record_path,
            serde_json::to_vec_pretty(&record).expect("serialize record"),
        )
        .expect("write record");

        let report = validate_catalog(
            directory.path(),
            Path::new(DEFAULT_CATALOG),
            true,
            true,
        )
        .expect("validate fixture");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.rule_id == "inventory.repository")
        );
    }

    #[cfg(unix)]
    #[test]
    fn catalog_intermediate_symlink_cannot_escape_the_superproject() {
        use std::os::unix::fs::symlink;

        let (directory, record_path) = fixture();
        let record = fs::read(&record_path).expect("read record");
        let outside = TempDir::new().expect("outside directory");
        fs::create_dir_all(outside.path().join("gitops/apps")).expect("outside catalog");
        fs::write(
            outside.path().join("gitops/apps/dd-fabrication-server.json"),
            record,
        )
        .expect("outside record");
        fs::remove_dir_all(directory.path().join("catalog")).expect("remove catalog");
        symlink(outside.path(), directory.path().join("catalog")).expect("catalog symlink");

        let error = validate_catalog(
            directory.path(),
            Path::new(DEFAULT_CATALOG),
            true,
            true,
        )
        .expect_err("escaping catalog must fail");
        assert!(error.to_string().contains("escapes the superproject root"));
    }

    #[cfg(unix)]
    #[test]
    fn static_application_parent_symlink_cannot_escape_the_superproject() {
        use std::os::unix::fs::symlink;

        let (directory, _) = fixture();
        let outside = TempDir::new().expect("outside directory");
        fs::write(
            outside.path().join("daedalus.applications.yaml"),
            "kind: Application\n",
        )
        .expect("outside static Application");
        fs::remove_dir_all(directory.path().join("remote/argocd/apps"))
            .expect("remove in-tree apps directory");
        symlink(
            outside.path(),
            directory.path().join("remote/argocd/apps"),
        )
        .expect("apps symlink");

        let report = validate_catalog(
            directory.path(),
            Path::new(DEFAULT_CATALOG),
            true,
            true,
        )
        .expect("validate fixture");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.rule_id == "migration.static-application-missing")
        );
    }

'''
tests.write_text(text.replace(marker, additions + marker, 1), encoding="utf-8")
