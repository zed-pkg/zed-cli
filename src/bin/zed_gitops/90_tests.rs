#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        DEFAULT_CATALOG, normalize_repository_url, validate_catalog, validate_relative_path,
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
}
