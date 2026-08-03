use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::{TempDir, tempdir};

const CLEAN_ENV: &[&str] = &[
    "CLASSPATH",
    "IN_NIX_SHELL",
    "PYTHONPATH",
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
    "ZED_PKG_AUTH_URL",
    "ZED_PKG_FROZEN",
    "ZED_PKG_HOME",
    "ZED_PKG_REGISTRY",
    "ZED_PKG_SUPABASE_KEY",
    "ZED_PKG_SUPABASE_URL",
    "ZED_PKG_TOKEN",
];

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempdir().expect("create test fixture");
        let root = temp.path().join("project");
        let home = temp.path().join("zed-home");
        fs::create_dir_all(&root).expect("create project root");
        fs::create_dir_all(&home).expect("create zed home");
        Self {
            _temp: temp,
            root,
            home,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
        command.current_dir(&self.root);
        for key in CLEAN_ENV {
            command.env_remove(key);
        }
        command.env("ZED_PKG_HOME", &self.home);
        command
    }

    fn native_project(&self) {
        fs::write(self.root.join("package.json"), "{}\n").expect("write package manifest");
    }

    fn print_env(&self, args: &[&str]) -> BTreeMap<String, String> {
        let mut command = self.command();
        command.args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ]);
        command.args(args);
        command.arg("--print-env");
        let output = command.output().expect("run zed dev --print-env");
        assert_success(&output);
        serde_json::from_slice(&output.stdout).expect("parse managed environment JSON")
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonicalize fixture path")
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
fn write_executable(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create executable parent");
    }
    fs::write(path, content).expect("write executable fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("mark executable fixture");
}

#[test]
fn global_options_before_the_alias_still_route_to_develop() {
    let fixture = Fixture::new();
    fixture.native_project();
    let explicit_home = fixture.root.join(".explicit-zed-home");

    let output = fixture
        .command()
        .args(["--registry=https://registry.example.invalid/", "--home"])
        .arg(&explicit_home)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--print-env",
        ])
        .output()
        .expect("run routed develop command");

    assert_success(&output);
    let environment: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("parse managed environment");
    assert_eq!(environment.get("ZED_DEV").map(String::as_str), Some("1"));
    let expected_root = canonical(&fixture.root);
    assert_eq!(
        environment.get("ZED_DEV_PROJECT_ROOT").map(String::as_str),
        Some(expected_root.as_str())
    );
}

#[test]
fn print_env_and_command_are_rejected_as_conflicting_modes() {
    let fixture = Fixture::new();
    fixture.native_project();

    let output = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--print-env",
            "-c",
            "true",
        ])
        .output()
        .expect("run conflicting develop modes");

    let stderr = assert_failure(&output);
    assert!(stderr.contains("--print-env"), "{stderr}");
    assert!(
        stderr.contains("--command") || stderr.contains("-c"),
        "{stderr}"
    );
}

#[test]
fn unknown_inline_option_values_are_not_echoed_back() {
    let fixture = Fixture::new();
    fixture.native_project();
    let secret = "should-never-appear-in-diagnostics";

    let output = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .arg(format!("--mystery={secret}"))
        .arg("--print-env")
        .output()
        .expect("run develop with unknown option");

    let stderr = assert_failure(&output);
    assert!(stderr.contains("--mystery"), "{stderr}");
    assert!(
        !stderr.contains(secret),
        "secret leaked in diagnostic: {stderr}"
    );
}

#[test]
fn invalid_boolean_environment_values_fail_closed() {
    let fixture = Fixture::new();
    fixture.native_project();

    let output = fixture
        .command()
        .env("ZED_DEV_NO_INSTALL", "sometimes")
        .args([
            "dev",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--print-env",
        ])
        .output()
        .expect("run develop with invalid boolean environment");

    let stderr = assert_failure(&output);
    assert!(stderr.contains("ZED_DEV_NO_INSTALL"), "{stderr}");
    assert!(stderr.contains("true/false"), "{stderr}");
}

#[test]
fn accepted_boolean_environment_spellings_drive_the_command() {
    let fixture = Fixture::new();
    fixture.native_project();

    let output = fixture
        .command()
        .env("ZED_DEV_NO_INSTALL", "yes")
        .env("ZED_DEV_NIX", "never")
        .env("ZED_DEV_PYTHON_VENV", "never")
        .env("ZED_DEV_PRINT_ENV", "on")
        .arg("dev")
        .output()
        .expect("run environment-configured develop command");

    assert_success(&output);
    let environment: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("parse managed environment");
    assert_eq!(environment.get("ZED_DEV").map(String::as_str), Some("1"));
}

#[test]
fn interactive_mode_requires_a_real_terminal() {
    let fixture = Fixture::new();
    fixture.native_project();

    let output = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .output()
        .expect("run non-terminal interactive develop command");

    let stderr = assert_failure(&output);
    assert!(stderr.contains("needs a real terminal"), "{stderr}");
    assert!(stderr.contains("-c <command>"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn command_mode_propagates_the_shell_exit_code() {
    let fixture = Fixture::new();
    fixture.native_project();

    let status = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--shell",
            "/bin/sh",
            "-c",
            "exit 37",
        ])
        .status()
        .expect("run child shell");

    assert_eq!(status.code(), Some(37));
}

#[test]
fn missing_shell_errors_name_the_selected_executable() {
    let fixture = Fixture::new();
    fixture.native_project();
    let missing = fixture.root.join("missing-shell");

    let output = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--shell",
        ])
        .arg(&missing)
        .args(["-c", "true"])
        .output()
        .expect("run missing shell");

    let stderr = assert_failure(&output);
    assert!(stderr.contains("starting development shell"), "{stderr}");
    let missing_display = missing.to_string_lossy();
    assert!(stderr.contains(missing_display.as_ref()), "{stderr}");
}

#[test]
fn unique_nested_projects_are_selected_but_ambiguous_roots_are_not_guessed() {
    let fixture = Fixture::new();
    let web = fixture.root.join("apps/web");
    fs::create_dir_all(&web).expect("create web project");
    fs::write(web.join("package.json"), "{}\n").expect("write web manifest");

    let one = fixture.print_env(&[]);
    let expected_web = canonical(&web);
    assert_eq!(
        one.get("ZED_DEV_PROJECT_ROOT").map(String::as_str),
        Some(expected_web.as_str())
    );

    let api = fixture.root.join("apps/api");
    fs::create_dir_all(&api).expect("create api project");
    fs::write(
        api.join("Cargo.toml"),
        "[package]\nname = \"api\"\nversion = \"0.0.0\"\n",
    )
    .expect("write api manifest");

    let ambiguous = fixture.print_env(&[]);
    let expected_root = canonical(&fixture.root);
    assert_eq!(
        ambiguous.get("ZED_DEV_PROJECT_ROOT").map(String::as_str),
        Some(expected_root.as_str())
    );
}

#[test]
fn project_discovery_ignores_generated_vcs_and_too_deep_directories() {
    let fixture = Fixture::new();

    for relative in [
        ".git/fixture",
        ".hg/fixture",
        ".jj/fixture",
        ".zed/fixture",
        "node_modules/fixture",
        "target/fixture",
        "zed_modules/fixture",
        "a/b/c/d/e",
    ] {
        let directory = fixture.root.join(relative);
        fs::create_dir_all(&directory).expect("create excluded project");
        fs::write(directory.join("package.json"), "{}\n").expect("write excluded native manifest");
    }

    let environment = fixture.print_env(&[]);
    let expected_root = canonical(&fixture.root);
    assert_eq!(
        environment.get("ZED_DEV_PROJECT_ROOT").map(String::as_str),
        Some(expected_root.as_str())
    );
}

#[test]
fn print_env_does_not_load_dotenv_files_or_dump_inherited_secrets() {
    let fixture = Fixture::new();
    fixture.native_project();
    fs::write(
        fixture.root.join(".env"),
        "DOTENV_SECRET=dotenv-secret-value\n",
    )
    .expect("write dotenv fixture");
    fs::write(
        fixture.root.join(".envrc"),
        "export DIRENV_SECRET=direnv-secret-value\n",
    )
    .expect("write direnv fixture");

    let output = fixture
        .command()
        .env("INHERITED_SECRET", "inherited-secret-value")
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--print-env",
        ])
        .output()
        .expect("print managed environment");

    assert_success(&output);
    let environment: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("parse managed environment");
    for key in ["DOTENV_SECRET", "DIRENV_SECRET", "INHERITED_SECRET"] {
        assert!(!environment.contains_key(key), "{key} unexpectedly printed");
    }
    for secret in [
        "dotenv-secret-value",
        "direnv-secret-value",
        "inherited-secret-value",
    ] {
        assert!(
            !environment.values().any(|value| value.contains(secret)),
            "{secret} unexpectedly printed"
        );
    }
}

#[test]
fn isolated_home_is_empty_and_project_local_instead_of_copying_credentials() {
    let fixture = Fixture::new();
    fixture.native_project();
    let source_home = fixture.root.join("source-home");
    fs::create_dir_all(source_home.join(".codex")).expect("create source credential directory");
    fs::write(
        source_home.join(".codex/credentials.json"),
        "{\"token\":\"do-not-copy\"}\n",
    )
    .expect("write source credentials");

    let output = fixture
        .command()
        .env("HOME", &source_home)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--isolated-home",
            "--print-env",
        ])
        .output()
        .expect("run isolated develop environment");

    assert_success(&output);
    let environment: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("parse managed environment");
    let isolated =
        fs::canonicalize(fixture.root.join(".zed/dev/home")).expect("canonicalize isolated HOME");
    let isolated_string = isolated.to_string_lossy().into_owned();
    assert_eq!(
        environment.get("HOME").map(String::as_str),
        Some(isolated_string.as_str())
    );
    assert!(isolated.is_dir());
    assert!(!isolated.join(".codex/credentials.json").exists());
}

#[test]
fn language_adapter_files_are_reflected_in_the_managed_environment() {
    let fixture = Fixture::new();
    fixture.native_project();
    let metadata = fixture.root.join(".zed");
    fs::create_dir_all(&metadata).expect("create zed metadata directory");

    let python_a = fixture.root.join("python-a");
    let python_b = fixture.root.join("python-b");
    let class_a = fixture.root.join("class-a");
    let class_b = fixture.root.join("class-b");
    let pythonpath = env::join_paths([&python_a, &python_b]).expect("join Python paths");
    let classpath = env::join_paths([&class_a, &class_b]).expect("join Java paths");
    fs::write(
        metadata.join("pythonpath"),
        pythonpath.to_string_lossy().as_bytes(),
    )
    .expect("write Python path adapter");
    fs::write(
        metadata.join("classpath"),
        classpath.to_string_lossy().as_bytes(),
    )
    .expect("write classpath adapter");
    fs::write(metadata.join("go.work"), "go 1.23\n").expect("write Go workspace adapter");

    let environment = fixture.print_env(&[]);
    let python_paths: Vec<PathBuf> = env::split_paths(OsStr::new(
        environment.get("PYTHONPATH").expect("PYTHONPATH"),
    ))
    .collect();
    let class_paths: Vec<PathBuf> =
        env::split_paths(OsStr::new(environment.get("CLASSPATH").expect("CLASSPATH"))).collect();

    assert_eq!(python_paths, vec![python_a, python_b]);
    assert_eq!(class_paths, vec![class_a, class_b]);
    let go_work = fs::canonicalize(metadata.join("go.work"))
        .expect("canonicalize Go workspace adapter")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        environment.get("GOWORK").map(String::as_str),
        Some(go_work.as_str())
    );
}

#[test]
fn cargo_adapter_configuration_is_copied_into_the_development_home() {
    let fixture = Fixture::new();
    fixture.native_project();
    let metadata = fixture.root.join(".zed");
    fs::create_dir_all(&metadata).expect("create zed metadata directory");
    let source = "[patch.crates-io]\nexample = { path = \"../example\" }\n";
    fs::write(metadata.join("cargo-paths.toml"), source).expect("write cargo adapter");

    fixture.print_env(&[]);

    let destination = fixture.root.join(".zed/dev/cargo/home/config.toml");
    assert_eq!(
        fs::read_to_string(destination).expect("read copied cargo adapter"),
        source
    );
}

#[cfg(unix)]
#[test]
fn custom_python_interpreters_can_create_relative_managed_venvs() {
    let fixture = Fixture::new();
    fixture.native_project();
    let python = fixture.root.join("bin/fake-python");
    write_executable(
        &python,
        "#!/bin/sh\nset -eu\ntest \"$1\" = -m\ntest \"$2\" = venv\nmkdir -p \"$3/bin\"\n",
    );

    let output = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "required",
            "--python",
        ])
        .arg(&python)
        .args(["--venv", ".custom/python", "--print-env"])
        .output()
        .expect("run custom Python venv creation");

    assert_success(&output);
    let environment: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("parse managed environment");
    let expected = fs::canonicalize(fixture.root.join(".custom/python"))
        .expect("canonicalize custom Python venv");
    let expected_string = expected.to_string_lossy().into_owned();
    assert_eq!(
        environment.get("VIRTUAL_ENV").map(String::as_str),
        Some(expected_string.as_str())
    );
    assert!(expected.join("bin").is_dir());
}

#[test]
fn required_python_mode_fails_for_an_explicit_missing_interpreter() {
    let fixture = Fixture::new();
    fixture.native_project();
    let missing = fixture.root.join("missing-python");

    let output = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "required",
            "--python",
        ])
        .arg(&missing)
        .args(["--venv", ".custom/python", "--print-env"])
        .output()
        .expect("run missing Python interpreter");

    let stderr = assert_failure(&output);
    assert!(stderr.contains("--python-venv required"), "{stderr}");
    let missing_display = missing.to_string_lossy();
    assert!(stderr.contains(missing_display.as_ref()), "{stderr}");
}

#[test]
fn malformed_existing_virtual_environments_fail_before_shell_startup() {
    let fixture = Fixture::new();
    fixture.native_project();
    fs::create_dir_all(fixture.root.join(".venv")).expect("create malformed venv");

    let output = fixture
        .command()
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "auto",
            "--print-env",
        ])
        .output()
        .expect("run malformed venv");

    let stderr = assert_failure(&output);
    assert!(
        stderr.contains("not a usable Python virtual environment"),
        "{stderr}"
    );
    assert!(stderr.contains(".venv"), "{stderr}");
}

#[test]
fn frozen_mode_without_a_zed_manifest_or_lockfile_fails_closed() {
    let fixture = Fixture::new();

    let output = fixture
        .command()
        .args([
            "dev",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--frozen",
            "--print-env",
        ])
        .output()
        .expect("run frozen manifestless develop command");

    let stderr = assert_failure(&output);
    assert!(stderr.contains("--frozen requires"), "{stderr}");
    assert!(
        stderr.contains(".zpkg.toml") || stderr.contains(".zpkg.lock"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn shell_name_controls_the_command_argument_protocol() {
    let fixture = Fixture::new();
    fixture.native_project();
    let shells = fixture.root.join("shells");
    let script = "printf sentinel";
    let cases: &[(&str, &[&str])] = &[
        ("bash", &["-c", script]),
        ("fish", &["-c", script]),
        ("pwsh", &["-NoLogo", "-Command", script]),
        ("cmd.exe", &["/D", "/S", "/C", script]),
        ("custom-shell", &["-c", script]),
    ];

    for (name, expected) in cases {
        let shell = shells.join(name);
        let capture = shells.join(format!("{name}.args"));
        write_executable(
            &shell,
            "#!/bin/sh\nset -eu\n: \"${CAPTURE:?}\"\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\n",
        );

        let output = fixture
            .command()
            .env("CAPTURE", &capture)
            .args([
                "dev",
                "--no-install",
                "--nix",
                "never",
                "--python-venv",
                "never",
                "--shell",
            ])
            .arg(&shell)
            .args(["-c", script])
            .output()
            .expect("run shell protocol fixture");

        assert_success(&output);
        let actual: Vec<String> = fs::read_to_string(&capture)
            .expect("read captured shell arguments")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            actual,
            expected
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            "unexpected arguments for {name}"
        );
    }
}

#[test]
fn ai_profile_path_is_opt_in_and_precedes_generic_development_bins() {
    let fixture = Fixture::new();
    fixture.native_project();

    let default_environment = fixture.print_env(&[]);
    let default_paths: Vec<PathBuf> = env::split_paths(OsStr::new(
        default_environment.get("PATH").expect("default PATH"),
    ))
    .collect();
    let ai = fs::canonicalize(fixture.root.join(".zed/dev/profiles/ai/bin"))
        .expect("canonicalize AI profile path");
    assert!(
        !default_paths.contains(&ai),
        "AI profile path must not be enabled by default"
    );

    let ai_environment = fixture.print_env(&["--profile", "ai"]);
    let ai_paths: Vec<PathBuf> =
        env::split_paths(OsStr::new(ai_environment.get("PATH").expect("AI PATH"))).collect();
    let generic = fs::canonicalize(fixture.root.join(".zed/dev/bin"))
        .expect("canonicalize generic development bin path");
    let ai_index = ai_paths
        .iter()
        .position(|path| path == &ai)
        .expect("AI path");
    let generic_index = ai_paths
        .iter()
        .position(|path| path == &generic)
        .expect("generic dev path");

    assert!(ai_index < generic_index);
    assert_eq!(
        ai_environment.get("ZED_DEV_PROFILE").map(String::as_str),
        Some("ai")
    );
}
