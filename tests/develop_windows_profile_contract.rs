#![cfg(windows)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const PROFILE_CANARY: &str = "ZED_POWERSHELL_PROFILE_CANARY_MUST_NOT_LOAD";
const PROFILE_ENV: &str = "ZED_TEST_POWERSHELL_PROFILE";

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn powershell() -> PathBuf {
    for candidate in ["pwsh.exe", "powershell.exe"] {
        let output = Command::new("where.exe")
            .arg(candidate)
            .output()
            .unwrap_or_else(|error| panic!("locating {candidate}: {error}"));
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(path) = stdout.lines().map(str::trim).find(|line| !line.is_empty()) {
            return PathBuf::from(path);
        }
    }
    panic!("neither pwsh.exe nor powershell.exe is available");
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn run_powershell(executable: &Path, source_home: &Path, script: &str) -> Output {
    Command::new(executable)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .env("HOME", source_home)
        .env("USERPROFILE", source_home)
        .env_remove(PROFILE_ENV)
        .output()
        .unwrap_or_else(|error| panic!("running {}: {error}", executable.display()))
}

fn discover_profile_paths(executable: &Path, source_home: &Path) -> Vec<PathBuf> {
    let script = r#"
$ErrorActionPreference = 'Stop'
$HOME
$PROFILE.CurrentUserAllHosts
$PROFILE.CurrentUserCurrentHost
"#;
    let output = run_powershell(executable, source_home, script);
    assert!(
        output.status.success(),
        "profile discovery failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    assert!(
        lines.len() >= 3,
        "unexpected profile discovery output: {lines:?}"
    );

    let expected_home = normalize(source_home);
    assert_eq!(
        normalize(&lines[0]),
        expected_home,
        "PowerShell HOME was not isolated"
    );
    let mut profiles = Vec::new();
    for profile in &lines[1..] {
        assert!(
            normalize(profile).starts_with(&expected_home),
            "refusing to write a profile outside the temporary home: {}",
            profile.display()
        );
        if !profiles.contains(profile) {
            profiles.push(profile.clone());
        }
    }
    profiles
}

fn write_profile(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("creating {}: {error}", parent.display()));
    }
    fs::write(
        path,
        format!("$env:{PROFILE_ENV} = '{PROFILE_CANARY}'\nWrite-Output '{PROFILE_CANARY}'\n"),
    )
    .unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

fn project_fixture() -> (TempDir, PathBuf, PathBuf) {
    let temporary = tempfile::tempdir().expect("create Windows shell fixture");
    let project = temporary.path().join("project");
    let source_home = temporary.path().join("source-home");
    fs::create_dir_all(project.join("src/nested")).expect("create nested project");
    fs::create_dir_all(&source_home).expect("create isolated source home");
    fs::write(project.join("package.json"), "{}\n").expect("write package.json");
    (temporary, project, source_home)
}

#[test]
fn powershell_command_mode_does_not_load_profiles_and_propagates_exit() {
    let (_temporary, project, source_home) = project_fixture();
    let shell = powershell();
    for profile in discover_profile_paths(&shell, &source_home) {
        write_profile(&profile);
    }

    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         if (Test-Path Env:{profile_env}) {{ throw 'PowerShell profile was loaded' }}; \
         if ($env:ZED_DEV -ne '1') {{ throw 'managed environment missing' }}; \
         $actual = (Get-Item -LiteralPath '.').FullName.TrimEnd('\\'); \
         $expected = (Get-Item -LiteralPath $env:ZED_DEV_PROJECT_ROOT).FullName.TrimEnd('\\'); \
         if (-not [String]::Equals($actual, $expected, [StringComparison]::OrdinalIgnoreCase)) {{ throw \"project root mismatch: $actual != $expected\" }}; \
         Write-Output 'windows-powershell-profile-safe'; exit 29",
        profile_env = PROFILE_ENV,
    );

    let output = Command::new(zed_bin())
        .current_dir(project.join("src/nested"))
        .env("HOME", &source_home)
        .env("USERPROFILE", &source_home)
        .env("ZED_PKG_HOME", project.join(".zed-pkg-home"))
        .env_remove("SHELL")
        .env_remove(PROFILE_ENV)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--mise",
            "never",
            "--python-venv",
            "never",
            "--shell",
        ])
        .arg(&shell)
        .args(["-c", &script])
        .output()
        .expect("run zed PowerShell command mode");

    assert_eq!(
        output.status.code(),
        Some(29),
        "child exit code was not propagated; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("windows-powershell-profile-safe"),
        "{combined}"
    );
    assert!(
        !combined.contains(PROFILE_CANARY),
        "profile canary leaked: {combined}"
    );
}
