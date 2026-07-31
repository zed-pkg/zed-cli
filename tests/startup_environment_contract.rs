use std::process::{Command, Output};

fn root_help(interactive_env: &str, explicit_interactive: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command.env("ZED_PKG_INTERACTIVE", interactive_env);
    if explicit_interactive {
        command.arg("--interactive");
    }
    command.arg("--help");
    command.output().expect("run zed root help")
}

#[test]
fn invalid_global_boolean_fails_before_modular_root_help() {
    let output = root_help("invalid", false);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("boolean environment variable `ZED_PKG_INTERACTIVE`")
            && stderr.contains("true/false"),
        "unexpected stderr: {stderr}"
    );
    assert!(output.stdout.is_empty(), "help must not print after failure");
}

#[test]
fn explicit_global_boolean_overrides_a_malformed_inherited_value() {
    let output = root_help("invalid", true);
    assert!(
        output.status.success(),
        "explicit --interactive should win: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--interactive"), "unexpected help: {stdout}");
}

#[test]
fn portable_boolean_environment_spellings_reach_root_help() {
    for value in ["true", "false", "1", "0", "yes", "no", "on", "off"] {
        let output = root_help(value, false);
        assert!(
            output.status.success(),
            "{value}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("--interactive"),
            "{value}: root help was not rendered"
        );
    }
}
