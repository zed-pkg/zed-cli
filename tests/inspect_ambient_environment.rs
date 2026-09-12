use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};

const MANIFEST: &str = r#"
[package]
org = "acme"
name = "inspect-ambient-fixture"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/inspect-ambient-fixture"
"#;

const AMBIENT_KEYS: &[&str] = &[
    "ZED_PKG_REGISTRY",
    "ZED_PKG_HOME",
    "ZED_PKG_TOKEN",
    "ZED_PKG_AUTH_URL",
    "ZED_PKG_SUPABASE_URL",
    "ZED_PKG_SUPABASE_KEY",
    "ZED_PKG_INTERACTIVE",
    "ZED_PKG_GIT_SUBMODULES",
    "ZED_PKG_NO_MIRRORS",
    "ZED_PKG_TRUST_MIRROR_METADATA",
    "ZED_PKG_R2_PUBLIC_BASE",
    "ZED_PKG_R2_PUBLIC_KEY",
    "ZED_PKG_SOURCE_FALLBACK",
    "ZED_PKG_GLOBAL_BIN_DIR",
    "ZED_PKG_COMMAND",
    "ZED_PKG_UNKNOWN_OPTIONS",
    "ZED_PKG_PARSE_ERRORS",
    "ZED_PKG_GRAPH_FORMAT",
    "ZED_PKG_GRAPH_METADATA_JSON",
    "ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST",
    "ZED_PKG_ALLOW_NO_MANIFEST",
];

fn inspect_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command
        .arg("inspect")
        .arg("--format")
        .arg("json")
        .arg("--root")
        .arg(root);
    for key in AMBIENT_KEYS {
        command.env_remove(key);
    }
    command
}

fn require_clean_success(output: &Output) {
    assert!(
        output.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "inspect wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "inspect must emit exactly one JSON document"
    );
}

#[test]
fn inspect_output_is_invariant_under_poisoned_runtime_environment() {
    let sandbox = tempfile::tempdir().unwrap();
    let project = sandbox.path().join("project");
    let poisoned_home = sandbox.path().join("poisoned-home-must-not-exist");
    fs::create_dir(&project).unwrap();
    fs::write(project.join(MANIFEST_FILE), MANIFEST).unwrap();
    fs::write(project.join(LOCKFILE_FILE), "version = 1\n").unwrap();
    let root = project.canonicalize().unwrap();

    let clean = inspect_command(&root).output().unwrap();
    require_clean_success(&clean);

    let poison = "SYNTHETIC_INSPECT_AMBIENT_POISON_MUST_NOT_ESCAPE";
    let mut command = inspect_command(&root);
    command
        .env("ZED_PKG_REGISTRY", "https://registry-poison.invalid")
        .env("ZED_PKG_HOME", &poisoned_home)
        .env("ZED_PKG_TOKEN", poison)
        .env("ZED_PKG_AUTH_URL", "https://auth-poison.invalid")
        .env("ZED_PKG_SUPABASE_URL", "https://supabase-poison.invalid")
        .env("ZED_PKG_SUPABASE_KEY", poison)
        .env("ZED_PKG_INTERACTIVE", "not-a-boolean")
        .env("ZED_PKG_GIT_SUBMODULES", "not-a-boolean")
        .env("ZED_PKG_NO_MIRRORS", "not-a-boolean")
        .env("ZED_PKG_TRUST_MIRROR_METADATA", "not-a-boolean")
        .env("ZED_PKG_R2_PUBLIC_BASE", "https://r2-poison.invalid")
        .env("ZED_PKG_R2_PUBLIC_KEY", poison)
        .env("ZED_PKG_SOURCE_FALLBACK", "not-a-boolean")
        .env("ZED_PKG_GLOBAL_BIN_DIR", sandbox.path().join("poison-bin"))
        .env("ZED_PKG_COMMAND", "global")
        .env("ZED_PKG_UNKNOWN_OPTIONS", "--poisoned-option")
        .env("ZED_PKG_PARSE_ERRORS", poison)
        .env("ZED_PKG_GRAPH_FORMAT", "poison-format")
        .env("ZED_PKG_GRAPH_METADATA_JSON", "not-a-boolean")
        .env("ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST", "not-a-boolean")
        .env("ZED_PKG_ALLOW_NO_MANIFEST", "not-a-boolean");

    let poisoned = command.output().unwrap();
    require_clean_success(&poisoned);

    assert_eq!(
        poisoned.stdout, clean.stdout,
        "inspect output must be byte-identical regardless of ordinary runtime configuration"
    );
    let rendered = String::from_utf8(poisoned.stdout).unwrap();
    assert!(!rendered.contains(poison));
    assert!(!rendered.contains("registry-poison.invalid"));
    assert!(!rendered.contains("auth-poison.invalid"));
    assert!(!rendered.contains("supabase-poison.invalid"));
    assert!(!rendered.contains("r2-poison.invalid"));
    assert!(
        !poisoned_home.exists(),
        "inspect must not create or read the poisoned ZED_PKG_HOME path"
    );
}
