#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        DEFAULT_CATALOG, OutputFormat, ValidateArgs, normalize_repository_url, run_validate,
        validate_catalog, validate_gitops, validate_relative_path,
    };

    const PIN: &str = "32be546f5ee020c1de3b099a47e6760d00e3f6e4";

    fn run_git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .expect("git must run");
        assert!(status.success(), "git failed: {args:?}");
    }

    fn fixture() -> (TempDir, PathBuf) {
        let directory = TempDir::new().expect("temporary directory");
        let root = directory.path();
        fs::create_dir_all(root.join(DEFAULT_CATALOG)).expect("catalog directory");
        fs::create_dir_all(root.join("remote/argocd/apps")).expect("Argo apps directory");
        fs::write(
            root.join("remote/argocd/apps/daedalus.applications.yaml"),
            "kind: Application\n",
        )
        .expect("static Application");
        fs::write(
            root.join(".gitmodules"),
            concat!(
                "[submodule \"remote/deployments/fabrication-server-rs\"]\n",
                "\tpath = remote/deployments/fabrication-server-rs\n",
                "\turl = git@github.com:daedalus-fab/fabrication-server.rs.git\n"
            ),
        )
        .expect(".gitmodules");
        run_git(root, &["init", "--quiet"]);
        run_git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!(
                    "160000,{PIN},remote/deployments/fabrication-server-rs"
                ),
            ],
        );
        let record_path = root.join(DEFAULT_CATALOG).join("dd-fabrication-server.json");
        let record = json!({
            "$schema": "../application.schema.json",
            "apiVersion": "oresoftware.dev/v1alpha1",
            "kind": "GitOpsApplication",
            "metadata": { "name": "dd-fabrication-server" },
            "spec": {
                "owner": "daedalus-fab",
                "inventory": {
                    "mode": "git-submodule",
                    "path": "remote/deployments/fabrication-server-rs",
                    "repository": "git@github.com:daedalus-fab/fabrication-server.rs.git",
                    "revision": PIN
                },
                "source": {
                    "mode": "direct-repository",
                    "repository": "https://github.com/daedalus-fab/fabrication-server.rs",
                    "targetRevision": PIN,
                    "path": "k8s",
                    "renderer": "kustomize"
                },
                "argo": {
                    "project": "daedalus",
                    "namespace": "daedalus",
                    "destinationServer": "https://kubernetes.default.svc",
                    "automated": false,
                    "prune": false,
                    "selfHeal": false
                },
                "migration": {
                    "phase": "pilot-inert",
                    "staticApplication": "remote/argocd/apps/daedalus.applications.yaml"
                }
            }
        });
        fs::write(
            &record_path,
            serde_json::to_vec_pretty(&record).expect("serialize record"),
        )
        .expect("catalog record");
        (directory, record_path)
    }

    #[test]
    fn repository_urls_canonicalize_across_supported_github_forms() {
        let identities = [
            "git@github.com:Daedalus-Fab/fabrication-server.rs.git",
            "ssh://git@github.com/daedalus-fab/fabrication-server.rs.git",
            "https://github.com/daedalus-fab/fabrication-server.rs",
        ]
        .map(normalize_repository_url);
        assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(
            identities[0],
            "github.com/daedalus-fab/fabrication-server.rs"
        );
    }

    #[test]
    fn relative_path_validation_rejects_escape_and_git_control_paths() {
        assert!(validate_relative_path("remote/deployments/app").is_ok());
        for value in ["../app", "remote\\app", "/absolute", "remote/.git/app"] {
            assert!(validate_relative_path(value).is_err(), "{value}");
        }
    }

    #[test]
    fn valid_record_matches_gitmodules_and_index_gitlink() {
        let (directory, _) = fixture();
        let report = validate_catalog(
            directory.path(),
            Path::new(DEFAULT_CATALOG),
            true,
            true,
        )
        .expect("validate fixture");
        assert!(report.valid, "{:?}", report.diagnostics);
        assert_eq!(report.records, 1);
        assert_eq!(report.errors, 0);
    }

    #[test]
    fn target_revision_drift_fails_closed() {
        let (directory, record_path) = fixture();
        let mut record: serde_json::Value = serde_json::from_slice(
            &fs::read(&record_path).expect("read record"),
        )
        .expect("parse record");
        record["spec"]["source"]["targetRevision"] =
            serde_json::Value::String("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into());
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
        assert!(!report.valid);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.rule_id == "source.pin-drift")
        );
    }

    #[test]
    fn online_mode_fails_instead_of_claiming_unimplemented_evidence() {
        let (directory, _) = fixture();
        let error = run_validate(ValidateArgs {
            root: directory.path().to_path_buf(),
            catalog: PathBuf::from(DEFAULT_CATALOG),
            schema: None,
            changed_from: None,
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

    #[test]
    fn strict_mode_rejects_unknown_fields() {
        let (directory, record_path) = fixture();
        let mut record: serde_json::Value = serde_json::from_slice(
            &fs::read(&record_path).expect("read record"),
        )
        .expect("parse record");
        record["spec"]["inventory"]["branch"] = serde_json::Value::String("main".into());
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
                .any(|item| item.rule_id == "catalog.unknown-field")
        );
    }

    const CONTRACT: &str = include_str!("testdata/gitlink-contract.v1alpha1.json");
    const GITMODULES_CLEAN: &str = include_str!("testdata/gitmodules.clean");
    const GITMODULES_UNEXPECTED: &str = include_str!("testdata/gitmodules.unexpected");
    const GITMODULES_FORBIDDEN: &str = include_str!("testdata/gitmodules.forbidden-suffix");
    const LIB_PIN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ROGUE_PIN: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const INFRA_PIN: &str = "dddddddddddddddddddddddddddddddddddddddd";

    fn gitlink_args(root: &Path) -> ValidateArgs {
        ValidateArgs {
            root: root.to_path_buf(),
            catalog: PathBuf::from(DEFAULT_CATALOG),
            schema: None,
            changed_from: None,
            format: OutputFormat::Json,
            strict: true,
            offline: true,
        }
    }

    fn write_contract(root: &Path) {
        fs::create_dir_all(root.join("catalog/gitops")).expect("schema directory");
        fs::write(
            root.join("catalog/gitops/gitlink-contract.v1alpha1.json"),
            CONTRACT,
        )
        .expect("write contract");
    }

    fn gitlink_repo(gitmodules: &str, links: &[(&str, &str)]) -> TempDir {
        let directory = TempDir::new().expect("temporary directory");
        let root = directory.path();
        write_contract(root);
        fs::write(root.join(".gitmodules"), gitmodules).expect("write .gitmodules");
        run_git(root, &["init", "--quiet"]);
        for (path, pin) in links {
            run_git(
                root,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{pin},{path}"),
                ],
            );
        }
        directory
    }

    fn commit(root: &Path, message: &str) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=zed",
                "-c",
                "user.email=zed@example.com",
                "commit",
                "--quiet",
                "-m",
                message,
            ])
            .status()
            .expect("git commit must run");
        assert!(status.success(), "git commit failed: {message}");
    }

    fn git_line(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git must run");
        assert!(output.status.success(), "git failed: {args:?}");
        String::from_utf8(output.stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_string()
    }

    #[test]
    fn gitlink_contract_accepts_approved_app_path_and_ignores_library_gitlinks() {
        let directory = gitlink_repo(
            GITMODULES_CLEAN,
            &[("remote/deployments/fabrication-server-rs", PIN)],
        );
        fs::write(
            directory.path().join(".gitmodules"),
            format!(
                "{GITMODULES_CLEAN}[submodule \"remote/libs/pg-defs\"]\n\tpath = remote/libs/pg-defs\n\turl = git@github.com:oresoftware/pg-defs.git\n"
            ),
        )
        .expect("append library submodule");
        run_git(
            directory.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{LIB_PIN},remote/libs/pg-defs"),
            ],
        );

        let report = validate_gitops(&gitlink_args(directory.path())).expect("validate");
        assert!(report.valid, "{:?}", report.diagnostics);
        assert_eq!(report.gitlinks, 2);
        assert_eq!(report.records, 0);
        assert_eq!(
            report.schema.as_deref(),
            Some("catalog/gitops/gitlink-contract.v1alpha1.json")
        );
    }

    #[test]
    fn gitlink_contract_rejects_unlisted_app_gitlink() {
        let directory = gitlink_repo(
            GITMODULES_UNEXPECTED,
            &[
                ("remote/deployments/fabrication-server-rs", PIN),
                ("remote/deployments/rogue-app", ROGUE_PIN),
                ("remote/libs/pg-defs", LIB_PIN),
            ],
        );
        let report = validate_gitops(&gitlink_args(directory.path())).expect("validate");
        assert!(!report.valid);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.rule_id == "gitlink.unexpected"
                    && item.path == "remote/deployments/rogue-app"),
            "{:?}",
            report.diagnostics
        );
        assert!(
            report
                .diagnostics
                .iter()
                .all(|item| item.path != "remote/libs/pg-defs"),
            "{:?}",
            report.diagnostics
        );
    }

    #[test]
    fn gitlink_contract_rejects_forbidden_app_suffix() {
        let directory = gitlink_repo(
            GITMODULES_FORBIDDEN,
            &[("remote/deployments/daedalus-infra", INFRA_PIN)],
        );
        let report = validate_gitops(&gitlink_args(directory.path())).expect("validate");
        assert!(!report.valid);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.rule_id == "gitlink.forbidden-suffix"),
            "{:?}",
            report.diagnostics
        );
    }

    #[test]
    fn gitlink_contract_rejects_untracked_git_directory_under_approved_prefix() {
        let directory = gitlink_repo(
            GITMODULES_CLEAN,
            &[("remote/deployments/fabrication-server-rs", PIN)],
        );
        let imposter = directory
            .path()
            .join("remote/deployments/imposter");
        fs::create_dir_all(&imposter).expect("imposter directory");
        fs::write(imposter.join(".git"), "gitdir: /tmp/imposter.git\n").expect("fake gitdir");

        let report = validate_gitops(&gitlink_args(directory.path())).expect("validate");
        assert!(!report.valid);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|item| item.rule_id == "gitlink.untracked"
                    && item.path == "remote/deployments/imposter"),
            "{:?}",
            report.diagnostics
        );
    }

    #[test]
    fn changed_from_reports_new_gitlink_against_origin_main() {
        let directory = TempDir::new().expect("temporary directory");
        let root = directory.path();
        write_contract(root);
        fs::write(root.join("README.md"), "base\n").expect("readme");
        run_git(root, &["init", "--quiet", "-b", "main"]);
        run_git(
            root,
            &[
                "add",
                "README.md",
                "catalog/gitops/gitlink-contract.v1alpha1.json",
            ],
        );
        commit(root, "base");
        let base = git_line(root, &["rev-parse", "HEAD"]);
        fs::write(root.join(".gitmodules"), GITMODULES_CLEAN).expect("write .gitmodules");
        run_git(root, &["add", ".gitmodules"]);
        run_git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{PIN},remote/deployments/fabrication-server-rs"),
            ],
        );
        commit(root, "add app gitlink");
        run_git(root, &["update-ref", "refs/remotes/origin/main", &base]);

        let mut args = gitlink_args(root);
        args.changed_from = Some("origin/main".into());
        let report = validate_gitops(&args).expect("validate");
        assert!(report.valid, "{:?}", report.diagnostics);
        assert_eq!(report.changed_from.as_deref(), Some("origin/main"));
        assert_eq!(
            report.changed_gitlinks,
            vec!["remote/deployments/fabrication-server-rs".to_string()]
        );
    }

    #[test]
    fn changed_from_missing_local_ref_fails_closed() {
        let directory = gitlink_repo(
            GITMODULES_CLEAN,
            &[("remote/deployments/fabrication-server-rs", PIN)],
        );
        let mut args = gitlink_args(directory.path());
        args.changed_from = Some("origin/main".into());
        let error = validate_gitops(&args).expect_err("missing ref must fail");
        assert!(
            error.to_string().contains("not a local commit"),
            "{error:#}"
        );
    }

    #[test]
    fn validate_help_lists_changed_from_and_schema() {
        use clap::Parser;

        let help = super::Cli::try_parse_from(["zed-gitops", "validate", "--help"])
            .expect_err("help exits before a match")
            .to_string();
        assert!(help.contains("--changed-from"), "{help}");
        assert!(help.contains("--schema"), "{help}");
        assert!(help.contains("--offline"), "{help}");
        assert!(help.contains("--strict"), "{help}");
    }
}
