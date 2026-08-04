use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const DEV_ENV_KEYS: &[&str] = &[
    "IN_NIX_SHELL",
    "ZED_DEV_NIX_ACTIVE",
    "ZED_DEV_COMMAND",
    "ZED_DEV_SHELL",
    "ZED_DEV_NIX",
    "ZED_DEV_PROFILE",
    "ZED_DEV_NO_INSTALL",
    "ZED_DEV_ISOLATED_HOME",
    "ZED_DEV_PRINT_ENV",
    "ZED_DEV_PYTHON_VENV",
    "ZED_DEV_PYTHON",
    "ZED_DEV_VENV",
    "ZED_PKG_REGISTRY",
    "ZED_PKG_HOME",
    "ZED_PKG_TOKEN",
    "ZED_PKG_AUTH_URL",
    "ZED_PKG_SUPABASE_URL",
    "ZED_PKG_SUPABASE_KEY",
    "ZED_PKG_FROZEN",
    "ZED_PKG_ALLOW_BUILD",
    "VIRTUAL_ENV",
    "UV_PROJECT_ENVIRONMENT",
    "PYTHONPATH",
    "CLASSPATH",
    "GOWORK",
];

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn package_project() -> TempDir {
    let project = tempfile::tempdir().expect("create project fixture");
    fs::write(project.path().join("package.json"), "{}\n").expect("write package.json");
    project
}

fn clean_command(root: &Path) -> Command {
    let mut command = Command::new(zed_bin());
    command.current_dir(root);
    for key in DEV_ENV_KEYS {
        command.env_remove(key);
    }
    command.env("ZED_PKG_HOME", root.join(".zed-pkg-home"));
    command
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, body).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

#[cfg(unix)]
fn capture_shell(path: &Path) {
    write_executable(
        path,
        r#"#!/bin/sh
set -eu
: "${ZED_TEST_CAPTURE:?missing ZED_TEST_CAPTURE}"
{
  printf 'cwd=%s\n' "$(pwd -P)"
  printf 'ZED_DEV=%s\n' "${ZED_DEV-}"
  printf 'ZED_DEV_PROJECT_ROOT=%s\n' "${ZED_DEV_PROJECT_ROOT-}"
  printf 'VIRTUAL_ENV=%s\n' "${VIRTUAL_ENV-}"
  printf 'UV_PROJECT_ENVIRONMENT=%s\n' "${UV_PROJECT_ENVIRONMENT-}"
  printf 'HOME=%s\n' "${HOME-}"
  printf 'PATH=%s\n' "${PATH-}"
  printf 'DOTENV=%s\n' "${ZED_TEST_DOTENV_SECRET-unset}"
  for argument in "$@"; do
    printf 'arg=%s\n' "$argument"
  done
} > "$ZED_TEST_CAPTURE"
exit "${ZED_TEST_EXIT_CODE:-0}"
"#,
    );
}

#[cfg(unix)]
fn capture_value(capture: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    capture
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
}

#[cfg(unix)]
fn capture_arguments(capture: &str) -> Vec<String> {
    capture
        .lines()
        .filter_map(|line| line.strip_prefix("arg=").map(str::to_owned))
        .collect()
}

#[cfg(unix)]
fn prepend_path(directory: &Path) -> OsString {
    let mut paths = vec![directory.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).expect("join PATH")
}

#[cfg(unix)]
#[test]
fn help_global_options_and_legacy_commands_route_without_regression() {
    let project = package_project();

    let before = clean_command(project.path())
        .arg("--home")
        .arg(project.path().join("home-before"))
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
            "printf route-before",
        ])
        .output()
        .expect("run global option before command");
    assert_success(&before);
    assert_eq!(stdout(&before), "route-before");

    let after = clean_command(project.path())
        .args(["dev", "--no-install", "--nix", "never"])
        .arg("--home")
        .arg(project.path().join("home-after"))
        .args([
            "--python-venv",
            "never",
            "--shell",
            "/bin/sh",
            "-c",
            "printf route-after",
        ])
        .output()
        .expect("run global option after command");
    assert_success(&after);
    assert_eq!(stdout(&after), "route-after");

    for arguments in [
        vec!["develop", "--help"],
        vec!["dev", "--help"],
        vec!["help", "develop"],
        vec!["help", "dev"],
    ] {
        let output = clean_command(project.path())
            .args(arguments)
            .output()
            .expect("render develop help");
        assert_success(&output);
        let help = stdout(&output);
        assert!(help.contains("--python-venv"));
        assert!(help.contains("--isolated-home"));
        assert!(help.contains("-c, --command"));
    }

    let root_help = clean_command(project.path())
        .arg("--help")
        .output()
        .expect("render root help");
    assert_success(&root_help);
    assert!(stdout(&root_help).contains("develop"));

    let completions = clean_command(project.path())
        .args(["completions", "bash"])
        .output()
        .expect("render legacy completion command");
    assert_success(&completions);
    let completion = stdout(&completions);
    assert!(completion.contains("develop"));
    assert!(completion.contains("dev"));
}

#[test]
fn invalid_enums_conflicts_and_boolean_environment_fail_closed() {
    let project = package_project();
    let cases = [
        (vec!["dev", "--nix", "sometimes"], "sometimes"),
        (vec!["dev", "--profile", "robots"], "robots"),
        (vec!["dev", "--python-venv", "occasionally"], "occasionally"),
        (
            vec!["dev", "--print-env", "-c", "true"],
            "cannot be used with",
        ),
    ];

    for (arguments, expected) in cases {
        let output = clean_command(project.path())
            .args(arguments)
            .output()
            .expect("run invalid parser case");
        assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
        assert!(
            stderr(&output).contains(expected),
            "expected {expected:?} in stderr:\n{}",
            stderr(&output)
        );
    }

    let invalid_boolean = clean_command(project.path())
        .env("ZED_DEV_NO_INSTALL", "definitely-not-a-bool")
        .args(["dev", "--help"])
        .output()
        .expect("run invalid boolean environment case");
    assert_eq!(invalid_boolean.status.code(), Some(1));
    let message = stderr(&invalid_boolean);
    assert!(message.contains("ZED_DEV_NO_INSTALL"));
    assert!(message.contains("true/false"));
}

#[test]
fn flags2env_unknown_inline_values_are_redacted() {
    let project = package_project();
    let secret = "INLINE_SECRET_MUST_NOT_APPEAR";
    let output = clean_command(project.path())
        .arg("dev")
        .arg(format!("--not-a-real-option={secret}"))
        .output()
        .expect("run unknown-option case");

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("--not-a-real-option=<redacted>"));
    assert!(
        !message.contains(secret),
        "secret leaked in stderr: {message}"
    );
}

#[cfg(unix)]
#[test]
fn project_root_discovery_honors_nearest_exclusions_depth_and_ambiguity() {
    let fixture = tempfile::tempdir().expect("create project-root fixture");
    let capture = fixture.path().join("capture.txt");
    let shell = fixture.path().join("capture-shell");
    capture_shell(&shell);

    let web = fixture.path().join("apps/web");
    fs::create_dir_all(web.join("src")).expect("create web project");
    fs::write(web.join("package.json"), "{}\n").expect("write web package.json");

    for ignored in [
        fixture.path().join("node_modules/ignored"),
        fixture.path().join(".zed/ignored"),
        fixture.path().join("target/ignored"),
        fixture.path().join("zed_modules/ignored"),
        fixture.path().join("one/two/three/four/five"),
    ] {
        fs::create_dir_all(&ignored).expect("create ignored project");
        fs::write(ignored.join("package.json"), "{}\n").expect("write ignored package.json");
    }

    let unique = clean_command(fixture.path())
        .env("ZED_TEST_CAPTURE", &capture)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .arg("--shell")
        .arg(&shell)
        .args(["-c", "ignored"])
        .output()
        .expect("run unique nested project");
    assert_success(&unique);
    let unique_capture = fs::read_to_string(&capture).expect("read unique capture");
    assert_eq!(
        capture_value(&unique_capture, "cwd"),
        Some(fs::canonicalize(&web).unwrap().display().to_string())
    );

    let api = fixture.path().join("libs/api");
    fs::create_dir_all(&api).expect("create second project");
    fs::write(
        api.join("Cargo.toml"),
        "[package]\nname = \"api\"\nversion = \"0.0.0\"\n",
    )
    .expect("write Cargo.toml");

    let ambiguous = clean_command(fixture.path())
        .env("ZED_TEST_CAPTURE", &capture)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .arg("--shell")
        .arg(&shell)
        .args(["-c", "ignored"])
        .output()
        .expect("run ambiguous project root");
    assert_success(&ambiguous);
    let ambiguous_capture = fs::read_to_string(&capture).expect("read ambiguous capture");
    assert_eq!(
        capture_value(&ambiguous_capture, "cwd"),
        Some(
            fs::canonicalize(fixture.path())
                .unwrap()
                .display()
                .to_string()
        )
    );

    fs::write(fixture.path().join("package.json"), "{}\n").expect("write root package.json");
    let nearest = clean_command(&web.join("src"))
        .env("ZED_TEST_CAPTURE", &capture)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .arg("--shell")
        .arg(&shell)
        .args(["-c", "ignored"])
        .output()
        .expect("run nearest ancestor project");
    assert_success(&nearest);
    let nearest_capture = fs::read_to_string(&capture).expect("read nearest capture");
    assert_eq!(
        capture_value(&nearest_capture, "cwd"),
        Some(fs::canonicalize(&web).unwrap().display().to_string())
    );
}

#[cfg(unix)]
#[test]
fn nix_reentry_uses_nearest_flake_forwards_options_and_keeps_token_off_argv() {
    let fixture = tempfile::tempdir().expect("create Nix fixture");
    let app = fixture.path().join("apps/web");
    fs::create_dir_all(app.join("src")).expect("create nested app");
    fs::write(app.join("package.json"), "{}\n").expect("write package.json");
    fs::write(fixture.path().join("flake.nix"), "{}\n").expect("write root flake");
    fs::create_dir_all(app.join(".nix")).expect("create nested .nix");
    fs::write(app.join(".nix/flake.nix"), "{}\n").expect("write nested flake");

    let bin = fixture.path().join("bin");
    fs::create_dir_all(&bin).expect("create fake bin");
    let nix = bin.join("nix");
    write_executable(
        &nix,
        r#"#!/bin/sh
set -eu
if [ "${1-}" = "--version" ]; then
  printf 'nix (fake) 2.0\n'
  exit 0
fi
: "${ZED_TEST_NIX_ARGS:?}"
: "${ZED_TEST_NIX_ENV:?}"
: > "$ZED_TEST_NIX_ARGS"
for argument in "$@"; do
  printf '%s\n' "$argument" >> "$ZED_TEST_NIX_ARGS"
done
printf '%s' "${ZED_PKG_TOKEN-}" > "$ZED_TEST_NIX_ENV"
exit 23
"#,
    );

    let args_log = fixture.path().join("nix-args.txt");
    let env_log = fixture.path().join("nix-env.txt");
    let token = "TOKEN_SENTINEL_NOT_FOR_ARGV";
    let output = clean_command(&app.join("src"))
        .env("PATH", prepend_path(&bin))
        .env("ZED_TEST_NIX_ARGS", &args_log)
        .env("ZED_TEST_NIX_ENV", &env_log)
        .arg("--token")
        .arg(token)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "required",
            "--profile",
            "ai",
            "--python-venv",
            "never",
            "--isolated-home",
            "-c",
            "ignored command",
        ])
        .output()
        .expect("run fake Nix reentry");

    assert_eq!(
        output.status.code(),
        Some(23),
        "stderr: {}",
        stderr(&output)
    );
    let arguments: Vec<String> = fs::read_to_string(&args_log)
        .expect("read Nix argv")
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(arguments.len() >= 10, "unexpected Nix argv: {arguments:?}");
    assert_eq!(arguments[0], "develop");
    assert_eq!(
        PathBuf::from(&arguments[1]),
        fs::canonicalize(&app).unwrap().join(".nix")
    );
    assert_eq!(arguments[2], "--command");
    assert_eq!(arguments[4], "develop");
    assert!(arguments.windows(2).any(|pair| pair == ["--nix", "never"]));
    assert!(arguments.windows(2).any(|pair| pair == ["--profile", "ai"]));
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--isolated-home")
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["-c", "ignored command"])
    );
    assert!(
        !arguments.iter().any(|argument| argument.contains(token)),
        "token leaked into Nix argv: {arguments:?}"
    );
    assert_eq!(fs::read_to_string(&env_log).unwrap(), token);
}

#[cfg(unix)]
#[test]
fn nix_auto_falls_back_but_required_without_a_flake_fails() {
    let project = package_project();
    let empty_bin = project.path().join("empty-bin");
    fs::create_dir_all(&empty_bin).expect("create empty PATH");
    fs::create_dir_all(project.path().join(".nix")).expect("create .nix");
    fs::write(project.path().join(".nix/flake.nix"), "{}\n").expect("write flake");

    let fallback = clean_command(project.path())
        .env("PATH", &empty_bin)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "auto",
            "--python-venv",
            "never",
            "--shell",
            "/bin/sh",
            "-c",
            "printf native-fallback",
        ])
        .output()
        .expect("run Nix auto fallback");
    assert_success(&fallback);
    assert_eq!(stdout(&fallback), "native-fallback");
    assert!(stderr(&fallback).contains("`nix` is unavailable"));

    fs::remove_file(project.path().join(".nix/flake.nix")).expect("remove flake");
    let required = clean_command(project.path())
        .args([
            "dev",
            "--no-install",
            "--nix",
            "required",
            "--python-venv",
            "never",
            "--print-env",
        ])
        .output()
        .expect("run missing required flake");
    assert_eq!(required.status.code(), Some(1));
    assert!(stderr(&required).contains("no `.nix/flake.nix` or `flake.nix`"));
}

#[cfg(unix)]
fn fake_python(path: &Path) {
    write_executable(
        path,
        r#"#!/bin/sh
set -eu
: "${ZED_TEST_PYTHON_ARGS:?missing ZED_TEST_PYTHON_ARGS}"
printf '%s\n' "$@" > "$ZED_TEST_PYTHON_ARGS"
test "${1-}" = "-m"
test "${2-}" = "venv"
mkdir -p "$3/bin"
"#,
    );
}

#[cfg(unix)]
#[test]
fn python_venv_modes_cover_relative_absolute_skip_and_invalid_paths() {
    let fixture = tempfile::tempdir().expect("create Python fixture");
    let shell = fixture.path().join("capture-shell");
    capture_shell(&shell);
    let python = fixture.path().join("fake-python");
    fake_python(&python);

    let relative_project = fixture.path().join("relative-project");
    fs::create_dir_all(&relative_project).expect("create relative project");
    fs::write(
        relative_project.join("pyproject.toml"),
        "[project]\nname = \"relative\"\nversion = \"0.0.0\"\n",
    )
    .expect("write relative pyproject");
    let relative_capture = fixture.path().join("relative-capture.txt");
    let relative_args = fixture.path().join("relative-python-args.txt");
    let relative = clean_command(&relative_project)
        .env("ZED_TEST_CAPTURE", &relative_capture)
        .env("ZED_TEST_PYTHON_ARGS", &relative_args)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "required",
        ])
        .arg("--python")
        .arg(&python)
        .args(["--venv", "custom/venv"])
        .arg("--shell")
        .arg(&shell)
        .args(["-c", "ignored"])
        .output()
        .expect("create relative venv");
    assert_success(&relative);
    let relative_root = fs::canonicalize(&relative_project).unwrap();
    let relative_venv = relative_root.join("custom/venv");
    let relative_text = fs::read_to_string(&relative_capture).unwrap();
    assert_eq!(
        capture_value(&relative_text, "VIRTUAL_ENV"),
        Some(relative_venv.display().to_string())
    );
    assert_eq!(
        capture_value(&relative_text, "UV_PROJECT_ENVIRONMENT"),
        Some(relative_venv.display().to_string())
    );
    assert!(relative_venv.join("bin").is_dir());
    let relative_python_args: Vec<String> = fs::read_to_string(&relative_args)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(relative_python_args[0..2], ["-m", "venv"]);
    assert_eq!(PathBuf::from(&relative_python_args[2]), relative_venv);

    let absolute_project = fixture.path().join("absolute-project");
    fs::create_dir_all(&absolute_project).expect("create absolute project");
    fs::write(
        absolute_project.join("pyproject.toml"),
        "[project]\nname = \"absolute\"\nversion = \"0.0.0\"\n",
    )
    .expect("write absolute pyproject");
    let absolute_venv = fixture.path().join("absolute-venv");
    let absolute_capture = fixture.path().join("absolute-capture.txt");
    let absolute_args = fixture.path().join("absolute-python-args.txt");
    let absolute = clean_command(&absolute_project)
        .env("ZED_TEST_CAPTURE", &absolute_capture)
        .env("ZED_TEST_PYTHON_ARGS", &absolute_args)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "required",
        ])
        .arg("--python")
        .arg(&python)
        .arg("--venv")
        .arg(&absolute_venv)
        .arg("--shell")
        .arg(&shell)
        .args(["-c", "ignored"])
        .output()
        .expect("create absolute venv");
    assert_success(&absolute);
    let absolute_text = fs::read_to_string(&absolute_capture).unwrap();
    assert_eq!(
        capture_value(&absolute_text, "VIRTUAL_ENV"),
        Some(absolute_venv.display().to_string())
    );

    let non_python = fixture.path().join("node-project");
    fs::create_dir_all(&non_python).expect("create non-Python project");
    fs::write(non_python.join("package.json"), "{}\n").expect("write package.json");
    let skip_marker = fixture.path().join("skip-python-args.txt");
    let skipped = clean_command(&non_python)
        .env("ZED_TEST_PYTHON_ARGS", &skip_marker)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "auto",
        ])
        .arg("--python")
        .arg(&python)
        .arg("--print-env")
        .output()
        .expect("skip Python venv for Node project");
    assert_success(&skipped);
    assert!(
        !skip_marker.exists(),
        "Python was invoked for a non-Python project"
    );
    let skipped_env: BTreeMap<String, String> = serde_json::from_slice(&skipped.stdout).unwrap();
    assert!(!skipped_env.contains_key("VIRTUAL_ENV"));

    let invalid_project = fixture.path().join("invalid-project");
    fs::create_dir_all(invalid_project.join(".venv")).expect("create invalid venv");
    fs::write(
        invalid_project.join("pyproject.toml"),
        "[project]\nname = \"invalid\"\nversion = \"0.0.0\"\n",
    )
    .expect("write invalid pyproject");
    let invalid = clean_command(&invalid_project)
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
        .expect("reject invalid existing venv");
    assert_eq!(invalid.status.code(), Some(1));
    assert!(stderr(&invalid).contains("not a usable Python virtual environment"));

    let missing = clean_command(&invalid_project)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "required",
        ])
        .arg("--python")
        .arg(fixture.path().join("definitely-missing-python"))
        .args(["--venv", "another-venv", "--print-env"])
        .output()
        .expect("reject missing required Python");
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("--python-venv required could not create"));
}

#[cfg(unix)]
#[test]
fn print_env_proves_path_order_adapters_directories_and_isolated_home() {
    let project = package_project();
    let root = fs::canonicalize(project.path()).unwrap();
    fs::create_dir_all(root.join(".zed")).expect("create .zed");

    let python_paths = [root.join("python-a"), root.join("python-b")];
    let class_paths = [root.join("java-a.jar"), root.join("java-b.jar")];
    let existing_python = root.join("python-existing");
    let existing_class = root.join("java-existing.jar");
    fs::write(
        root.join(".zed/pythonpath"),
        env::join_paths(&python_paths)
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .expect("write pythonpath adapter");
    fs::write(
        root.join(".zed/classpath"),
        env::join_paths(&class_paths)
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .expect("write classpath adapter");
    fs::write(root.join(".zed/go.work"), "go 1.23\n").expect("write go.work adapter");
    fs::write(root.join(".zed/cargo-paths.toml"), "paths = []\n").expect("write cargo adapter");

    let inherited_path = env::join_paths([
        root.join("node_modules/.bin"),
        root.join(".zed/dev/bin"),
        PathBuf::from("/usr/bin"),
    ])
    .unwrap();
    let output = clean_command(project.path())
        .env("PATH", inherited_path)
        .env("PYTHONPATH", env::join_paths([&existing_python]).unwrap())
        .env("CLASSPATH", env::join_paths([&existing_class]).unwrap())
        .env("INHERITED_SECRET", "must-not-be-serialized")
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--profile",
            "ai",
            "--isolated-home",
            "--print-env",
        ])
        .output()
        .expect("print managed environment");
    assert_success(&output);

    let managed: BTreeMap<String, String> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(managed.get("ZED_DEV").map(String::as_str), Some("1"));
    assert_eq!(
        managed.get("ZED_DEV_PROFILE").map(String::as_str),
        Some("ai")
    );
    assert!(!managed.contains_key("INHERITED_SECRET"));
    assert!(
        !managed
            .values()
            .any(|value| value == "must-not-be-serialized")
    );

    let paths: Vec<PathBuf> = env::split_paths(OsStr::new(managed.get("PATH").unwrap())).collect();
    let expected_prefix = [
        root.join("zed_modules/.bin"),
        root.join("node_modules/.bin"),
        root.join(".zed/dev/profiles/ai/bin"),
        root.join(".zed/dev/bin"),
        root.join(".zed/dev/node/pnpm"),
        root.join(".zed/dev/node/prefix/bin"),
        root.join(".zed/dev/cargo/home/bin"),
        root.join(".zed/dev/go/bin"),
        root.join(".zed/dev/ruby/gems/bin"),
    ];
    assert_eq!(&paths[..expected_prefix.len()], &expected_prefix);
    assert_eq!(
        paths
            .iter()
            .filter(|path| **path == root.join("node_modules/.bin"))
            .count(),
        1
    );
    assert_eq!(
        paths
            .iter()
            .filter(|path| **path == root.join(".zed/dev/bin"))
            .count(),
        1
    );

    assert_eq!(
        managed["HOME"],
        root.join(".zed/dev/home").display().to_string()
    );
    assert_eq!(
        managed["XDG_CONFIG_HOME"],
        root.join(".zed/dev/xdg/config").display().to_string()
    );
    assert_eq!(
        managed["XDG_DATA_HOME"],
        root.join(".zed/dev/xdg/data").display().to_string()
    );
    assert_eq!(
        managed["GOWORK"],
        root.join(".zed/go.work").display().to_string()
    );
    assert_eq!(managed["PYTHONNOUSERSITE"], "1");

    let actual_python: Vec<PathBuf> =
        env::split_paths(OsStr::new(managed.get("PYTHONPATH").unwrap())).collect();
    assert_eq!(
        actual_python,
        vec![
            python_paths[0].clone(),
            python_paths[1].clone(),
            existing_python
        ]
    );
    let actual_class: Vec<PathBuf> =
        env::split_paths(OsStr::new(managed.get("CLASSPATH").unwrap())).collect();
    assert_eq!(
        actual_class,
        vec![
            class_paths[0].clone(),
            class_paths[1].clone(),
            existing_class
        ]
    );

    assert_eq!(
        fs::read_to_string(root.join(".zed/dev/cargo/home/config.toml")).unwrap(),
        "paths = []\n"
    );
    for directory in [
        ".zed/dev/cargo/target",
        ".zed/dev/go/pkg/mod",
        ".zed/dev/node/corepack",
        ".zed/dev/python/cache/pip",
        ".zed/dev/dart/pub-cache",
        ".zed/dev/java/gradle",
        ".zed/dev/ruby/gems/bin",
        ".zed/dev/profiles/ai/bin",
        ".zed/dev/xdg/state",
    ] {
        assert!(
            root.join(directory).is_dir(),
            "missing managed directory {directory}"
        );
    }
}

#[cfg(unix)]
#[test]
fn shell_dispatch_covers_posix_fish_powershell_cmd_and_generic_shells() {
    let project = package_project();
    let script = "echo shell-matrix";
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("bash", vec!["-c", script]),
        ("fish", vec!["-c", script]),
        ("PwSh.ExE", vec!["-NoLogo", "-Command", script]),
        ("CMD.EXE", vec!["/D", "/S", "/C", script]),
        ("custom-shell", vec!["-c", script]),
    ];

    for (name, expected) in cases {
        let shell = project.path().join(name);
        let capture = project.path().join(format!("{name}.capture"));
        capture_shell(&shell);
        let output = clean_command(project.path())
            .env("ZED_TEST_CAPTURE", &capture)
            .args([
                "dev",
                "--no-install",
                "--nix",
                "never",
                "--python-venv",
                "never",
            ])
            .arg("--shell")
            .arg(&shell)
            .args(["-c", script])
            .output()
            .unwrap_or_else(|error| panic!("run shell case {name}: {error}"));
        assert_success(&output);
        let captured = fs::read_to_string(&capture).expect("read shell capture");
        assert_eq!(capture_arguments(&captured), expected, "shell {name}");
    }
}

#[cfg(unix)]
#[test]
fn child_exit_missing_shell_no_tty_and_dotenv_boundaries_fail_safely() {
    let project = package_project();
    let shell = project.path().join("capture-shell");
    let capture = project.path().join("capture.txt");
    capture_shell(&shell);

    let child_exit = clean_command(project.path())
        .env("ZED_TEST_CAPTURE", &capture)
        .env("ZED_TEST_EXIT_CODE", "37")
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .arg("--shell")
        .arg(&shell)
        .args(["-c", "ignored"])
        .output()
        .expect("propagate child exit code");
    assert_eq!(child_exit.status.code(), Some(37));

    let missing_path = project.path().join("definitely-missing-shell");
    let missing = clean_command(project.path())
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .arg("--shell")
        .arg(&missing_path)
        .args(["-c", "ignored"])
        .output()
        .expect("run missing shell case");
    assert_eq!(missing.status.code(), Some(1));
    let missing_message = stderr(&missing);
    assert!(missing_message.contains("starting development shell"));
    assert!(missing_message.contains(&missing_path.display().to_string()));

    fs::remove_file(&capture).ok();
    let no_tty = clean_command(project.path())
        .env("ZED_TEST_CAPTURE", &capture)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
        ])
        .arg("--shell")
        .arg(&shell)
        .output()
        .expect("run non-TTY interactive case");
    assert_eq!(no_tty.status.code(), Some(1));
    assert!(stderr(&no_tty).contains("needs a real terminal"));
    assert!(!capture.exists(), "shell spawned despite the no-TTY guard");

    fs::write(
        project.path().join(".env"),
        "ZED_TEST_DOTENV_SECRET=from-dotenv\n",
    )
    .expect("write .env");
    fs::write(
        project.path().join(".envrc"),
        "export ZED_TEST_DOTENV_SECRET=from-envrc\n",
    )
    .expect("write .envrc");
    fs::create_dir_all(project.path().join("env")).expect("create env directory");
    fs::write(
        project.path().join("env/.prod.env"),
        "ZED_TEST_DOTENV_SECRET=from-production\n",
    )
    .expect("write production env");
    let dotenv = clean_command(project.path())
        .env_remove("ZED_TEST_DOTENV_SECRET")
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
            "test -z \"${ZED_TEST_DOTENV_SECRET+x}\"; printf dotenv-safe",
        ])
        .output()
        .expect("verify dotenv boundary");
    assert_success(&dotenv);
    assert_eq!(stdout(&dotenv), "dotenv-safe");
}
