use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use zed_cli::nix_export_bundle::render_nix_export_bundle;

use super::common::{artifact, flake_lock, plan};

#[test]
#[ignore = "requires a pinned Nix installation and one explicit closure-acquisition step"]
fn generated_flake_checks_and_builds_offline_after_archive() {
    let current_system = Command::new("nix")
        .args([
            "eval",
            "--impure",
            "--raw",
            "--expr",
            "builtins.currentSystem",
        ])
        .output()
        .expect("querying builtins.currentSystem");
    assert!(current_system.status.success());
    let current_system = String::from_utf8(current_system.stdout).unwrap();

    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let mut plan = plan(&artifact, BTreeMap::new());
    plan.intent.systems = vec![current_system];
    let rendered = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();
    let root = tempfile::tempdir().unwrap();
    for (relative, bytes) in rendered.files {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    let command = |args: &[&str]| {
        let output = Command::new("nix")
            .args(args)
            .current_dir(root.path())
            .env(
                "NIX_CONFIG",
                "experimental-features = nix-command flakes\naccept-flake-config = false",
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "nix command failed: nix {}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    };

    // Input acquisition and output realization are explicit preparation
    // boundaries. A fresh Nix store cannot replay stdenv offline until the
    // immutable locked closure has been obtained once from configured caches.
    command(&["flake", "archive", "--no-update-lock-file"]);
    let primed = command(&[
        "build",
        "--no-update-lock-file",
        "--no-link",
        "--print-build-logs",
        "--print-out-paths",
        ".#sample",
    ]);
    let primed_path = String::from_utf8(primed.stdout).unwrap();

    command(&[
        "flake",
        "check",
        "--offline",
        "--no-update-lock-file",
        "--print-build-logs",
    ]);
    let replay = command(&[
        "build",
        "--offline",
        "--no-update-lock-file",
        "--no-link",
        "--print-build-logs",
        "--print-out-paths",
        ".#sample",
    ]);
    let replay_path = String::from_utf8(replay.stdout).unwrap();
    assert_eq!(primed_path.trim(), replay_path.trim());
}
