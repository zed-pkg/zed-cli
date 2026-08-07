use std::process::{Command, Output};

const CLEAN_ENV: &[&str] = &[
    "ZED_DEV_COMMAND",
    "ZED_DEV_ISOLATED_HOME",
    "ZED_DEV_NIX",
    "ZED_DEV_NIX_ACTIVE",
    "ZED_DEV_NO_INSTALL",
    "ZED_DEV_PRINT_ENV",
    "ZED_DEV_PROFILE",
    "ZED_DEV_PYTHON",
    "ZED_DEV_PYTHON_VENV",
    "ZED_DEV_SHELL",
    "ZED_DEV_VENV",
    "ZED_PKG_ALLOW_BUILD",
    "ZED_PKG_FROZEN",
];

fn zed(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    for key in CLEAN_ENV {
        command.env_remove(key);
    }
    command.args(args).output().expect("run zed CLI")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) -> String {
    let text = combined(output);
    assert!(
        output.status.success(),
        "command failed with {:?}:\n{text}",
        output.status.code()
    );
    text
}

fn assert_failure(output: &Output) -> String {
    let text = combined(output);
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded:\n{text}"
    );
    text
}

#[test]
fn every_help_spelling_exposes_the_same_develop_boundary() {
    let cases: &[&[&str]] = &[
        &["develop", "--help"],
        &["dev", "--help"],
        &["help", "develop"],
        &["help", "dev"],
    ];

    for args in cases {
        let text = assert_success(&zed(args));
        for expected in [
            "package-aware",
            "--command",
            "--nix",
            "--profile",
            "--isolated-home",
            "--print-env",
            "--python-venv",
            "--venv",
        ] {
            assert!(
                text.contains(expected),
                "help for {args:?} omitted {expected}:\n{text}"
            );
        }
    }
}

#[test]
fn root_help_advertises_the_canonical_command_and_alias() {
    let text = assert_success(&zed(&["--help"]));
    assert!(
        text.contains("develop"),
        "root help omitted develop:\n{text}"
    );
    assert!(text.contains("dev"), "root help omitted dev alias:\n{text}");
    assert!(text.contains("virtual development"), "{text}");
}

#[test]
fn global_options_before_develop_help_still_select_the_develop_parser() {
    let text = assert_success(&zed(&[
        "--registry=https://registry.example.invalid/",
        "--home=.zed-help-home",
        "dev",
        "--help",
    ]));
    assert!(text.contains("package-aware"), "{text}");
    assert!(text.contains("--python-venv"), "{text}");
    assert!(text.contains("--isolated-home"), "{text}");
}

#[test]
fn develop_help_is_side_effect_free() {
    let directory = tempfile::tempdir().expect("create help fixture");
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command.current_dir(directory.path());
    for key in CLEAN_ENV {
        command.env_remove(key);
    }
    command.args(["dev", "--help"]);

    let output = command.output().expect("run develop help");
    let text = assert_success(&output);
    assert!(text.contains("package-aware"), "{text}");
    assert!(!directory.path().join(".zed").exists());
    assert!(!directory.path().join(".zpkg.toml").exists());
    assert!(!directory.path().join(".zpkg.lock").exists());
}

#[test]
fn legacy_command_help_is_not_polluted_by_develop_only_flags() {
    let text = assert_success(&zed(&["install", "--help"]));
    assert!(text.contains("install"), "{text}");
    for develop_only in ["--isolated-home", "--print-env", "--python-venv", "--venv"] {
        assert!(
            !text.contains(develop_only),
            "legacy install help unexpectedly contains {develop_only}:\n{text}"
        );
    }
}

#[test]
fn invalid_develop_enum_values_fail_before_environment_setup() {
    let cases: &[(&[&str], &str)] = &[
        (&["dev", "--nix", "sometimes"], "--nix"),
        (&["dev", "--profile", "robot"], "--profile"),
        (&["dev", "--python-venv", "maybe"], "--python-venv"),
    ];

    for (args, expected_option) in cases {
        let text = assert_failure(&zed(args));
        assert!(
            text.contains(expected_option),
            "failure for {args:?} omitted {expected_option}:\n{text}"
        );
        assert!(
            text.contains("invalid") || text.contains("possible values"),
            "failure for {args:?} was not actionable:\n{text}"
        );
    }
}

#[test]
fn root_version_and_legacy_help_continue_through_the_existing_cli() {
    let version = assert_success(&zed(&["--version"]));
    assert!(version.contains("zed"), "{version}");

    let legacy_help = assert_success(&zed(&["help", "install"]));
    assert!(legacy_help.contains("install"), "{legacy_help}");
    assert!(!legacy_help.contains("--python-venv"), "{legacy_help}");
}
