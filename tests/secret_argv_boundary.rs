use std::fs;
use std::process::Command;

const CLI_CONTRACTS: &[&str] = &[
    ".cli-flags.toml",
    ".dev-cli-flags.toml",
    ".fetch-cli-flags.toml",
    ".nix-interop-cli-flags.toml",
    ".task-cli-flags.toml",
    ".tool-cli-flags.toml",
];

const SECRET_VALUE_ENVS: &[&str] = &["ZED_PKG_TOKEN", "ZED_PKG_AUTH_PASSWORD"];

fn reject_secret_flag_envs(file: &str, value: &toml::Value, path: &str) {
    let Some(table) = value.as_table() else {
        return;
    };

    if let Some(flags) = table.get("flags").and_then(toml::Value::as_table) {
        for (name, flag) in flags {
            let env = flag
                .get("env")
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("{file}:{path}.flags.{name} is missing env"));
            assert!(
                !SECRET_VALUE_ENVS.contains(&env),
                "{file}:{path}.flags.{name} exposes secret-bearing {env} through argv; keep the value env/session/stdin-only"
            );
        }
    }

    for (name, child) in table {
        if name == "flags" {
            continue;
        }
        let child_path = if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}.{name}")
        };
        reject_secret_flag_envs(file, child, &child_path);
    }
}

#[test]
fn secret_values_are_not_public_flags_in_any_cli_contract() {
    for file in CLI_CONTRACTS {
        let text =
            fs::read_to_string(file).unwrap_or_else(|error| panic!("reading {file}: {error}"));
        let doc: toml::Value =
            toml::from_str(&text).unwrap_or_else(|error| panic!("parsing {file}: {error}"));
        reject_secret_flag_envs(file, &doc, "");
    }
}

#[test]
fn stdin_boolean_controls_remain_public_without_exposing_secret_values() {
    let text = fs::read_to_string(".cli-flags.toml").expect("reading .cli-flags.toml");
    let doc: toml::Value = toml::from_str(&text).expect("parsing .cli-flags.toml");
    let flags = doc
        .get("flags")
        .and_then(toml::Value::as_table)
        .expect("root flags table");

    let password_stdin = flags
        .get("password_stdin")
        .and_then(toml::Value::as_table)
        .expect("password_stdin flag remains public");
    assert_eq!(
        password_stdin.get("env").and_then(toml::Value::as_str),
        Some("ZED_PKG_AUTH_PASSWORD_STDIN")
    );
    assert_eq!(
        password_stdin.get("type").and_then(toml::Value::as_str),
        Some("bool")
    );
}

#[test]
fn root_help_does_not_bypass_rejected_bearer_token_argv() {
    let secret = "SYNTHETIC_BEARER_MUST_NOT_ECHO";
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["--token", secret, "--help"])
        .env_remove("ZED_PKG_TOKEN")
        .output()
        .expect("run zed with rejected bearer-token argv");

    assert!(
        !output.status.success(),
        "--token must be rejected before root help can return success"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains(secret),
        "secret leaked to stdout: {stdout}"
    );
    assert!(
        !stderr.contains(secret),
        "secret leaked to stderr: {stderr}"
    );
}
