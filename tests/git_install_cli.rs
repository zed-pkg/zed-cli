//! Real Git/Cargo acceptance tests for GH-333; no registry or network is used.
//! The TEST HARNESS isolates its environment, not the production installer.
//! Source-build trust and the installer's flags-2-env gate remain separate.
#![cfg(unix)]

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const NAME: &str = "fixture-cli";
const SOURCE: &str = "https://fixture.invalid/source.git";
const MANIFEST: &str = "[package]\nname = 'fixture-cli'\nversion = '0.1.0'\nlanguage = 'rust'\n[package.repository]\ntype = 'git'\nurl = 'https://fixture.invalid/source.git'\n[bin]\nfixture-cli = 'target/release/fixture-cli'\n";

struct Fixture {
    root: tempfile::TempDir,
    repo: PathBuf,
    revision: String,
}

fn successful(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

impl Fixture {
    fn new() -> Self {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        fs::create_dir_all(&scratch).unwrap();
        let root = tempfile::Builder::new()
            .prefix("git-install-cli-")
            .tempdir_in(scratch)
            .unwrap();
        let repo = root.path().join("source");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        fs::create_dir_all(root.path().join("cargo-home")).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[workspace]\n[package]\nname = 'fixture-cli'\nversion = '0.1.0'\nedition = '2021'\n",
        )
        .unwrap();
        fs::write(
            repo.join("src/main.rs"),
            "fn main() { println!(\"first\"); }\n",
        )
        .unwrap();
        fs::write(repo.join(".zpkg.toml"), MANIFEST).unwrap();
        let mut fixture = Self {
            root,
            repo,
            revision: String::new(),
        };
        successful(
            fixture
                .tool("cargo")
                .current_dir(&fixture.repo)
                .args(["generate-lockfile", "--offline"])
                .output()
                .unwrap(),
        );
        fixture.git(&["init", "--quiet", "--template=", "--initial-branch=main"]);
        fixture.commit(&["Cargo.toml", "Cargo.lock", ".zpkg.toml", "src/main.rs"]);
        fixture
    }

    fn tool(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.env_clear().stdin(Stdio::null());
        for key in ["PATH", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "TMPDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        // A fresh HOME/CARGO_HOME must not consult the runner's Git credentials
        // or Cargo registry credentials. Rustup still needs its installed tools.
        if std::env::var_os("RUSTUP_HOME").is_none() {
            let home = std::env::var_os("HOME").expect("Rustup fixture requires HOME");
            command.env("RUSTUP_HOME", PathBuf::from(home).join(".rustup"));
        }
        command
            .env("HOME", self.root.path().join("home"))
            .env("CARGO_HOME", self.root.path().join("cargo-home"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_BUILD_JOBS", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_DEFAULT_HASH", "sha1")
            .env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("url.file://{}.insteadOf", self.repo.display()),
            )
            .env("GIT_CONFIG_VALUE_0", SOURCE);
        command
    }

    fn git(&self, arguments: &[&str]) -> Output {
        successful(
            self.tool("git")
                .current_dir(&self.repo)
                .args(arguments)
                .output()
                .unwrap(),
        )
    }

    fn commit(&mut self, paths: &[&str]) {
        let mut add = vec!["add", "--"];
        add.extend_from_slice(paths);
        self.git(&add);
        self.git(&[
            "-c",
            "user.name=Installer test fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "dependency-free test fixture",
        ]);
        self.revision = String::from_utf8(self.git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
    }

    fn destination(&self) -> PathBuf {
        self.root.path().join("global bins").join(NAME)
    }

    fn receipts(&self) -> PathBuf {
        self.root.path().join("state/global/git-installs")
    }

    fn receipt(&self) -> PathBuf {
        self.receipts()
            .join(format!("{NAME}-{}.json", &self.revision[..12]))
    }

    fn installer(&self, revision: &str) -> Command {
        let mut command = self.tool(env!("CARGO_BIN_EXE_zed-git-install"));
        command
            .current_dir(self.root.path())
            .args(["--git", SOURCE, "--rev", revision, "--bin", NAME])
            .arg("--global-bin-dir")
            .arg(self.destination().parent().unwrap())
            .arg("--home")
            .arg(self.root.path().join("state"));
        command
    }

    fn install(&self, force: bool) -> Output {
        let mut command = self.installer(&self.revision);
        if force {
            command.arg("--force");
        }
        command.output().unwrap()
    }

    fn old_binary(&self) {
        fs::create_dir_all(self.destination().parent().unwrap()).unwrap();
        fs::write(self.destination(), b"original executable").unwrap();
    }

    fn rejected(&self, output: Output, diagnostic: &str) {
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "unexpected error: {stderr}");
        assert!(!self.receipts().exists(), "failed install wrote a receipt");
    }
}

#[test]
fn real_git_and_cargo_install_matches_receipt_and_executable() {
    let fixture = Fixture::new();
    successful(fixture.install(false));
    let output = successful(Command::new(fixture.destination()).output().unwrap());
    assert_eq!(output.stdout, b"first\n");
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.receipt()).unwrap()).unwrap();
    assert_eq!(receipt["revision"], fixture.revision);
    assert_eq!(receipt["source"], SOURCE);
    assert_eq!(receipt["binary"], NAME);
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(
        receipt["installed_path"],
        fixture.destination().to_str().unwrap()
    );
    assert_eq!(
        receipt["sha256"],
        hex::encode(Sha256::digest(fs::read(fixture.destination()).unwrap()))
    );
}

#[test]
fn repeated_non_force_install_preserves_binary_and_receipt() {
    let fixture = Fixture::new();
    successful(fixture.install(false));
    let binary = fs::read(fixture.destination()).unwrap();
    let receipt = fs::read(fixture.receipt()).unwrap();
    let output = fixture.install(false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert_eq!(fs::read(fixture.destination()).unwrap(), binary);
    assert_eq!(fs::read(fixture.receipt()).unwrap(), receipt);
}

#[test]
fn forced_new_revision_replaces_binary_and_retains_prior_receipt() {
    let mut fixture = Fixture::new();
    successful(fixture.install(false));
    let old_receipt_path = fixture.receipt();
    let old_receipt = fs::read(&old_receipt_path).unwrap();
    fs::write(
        fixture.repo.join("src/main.rs"),
        "fn main() { println!(\"second\"); }\n",
    )
    .unwrap();
    fixture.commit(&["src/main.rs"]);
    successful(fixture.install(true));
    let output = successful(Command::new(fixture.destination()).output().unwrap());
    assert_eq!(output.stdout, b"second\n");
    assert_eq!(fs::read(old_receipt_path).unwrap(), old_receipt);
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.receipt()).unwrap()).unwrap();
    assert_eq!(receipt["revision"], fixture.revision);
    assert_eq!(
        receipt["sha256"],
        hex::encode(Sha256::digest(fs::read(fixture.destination()).unwrap()))
    );
}

#[test]
fn failed_cargo_build_preserves_existing_destination_without_receipt() {
    let mut fixture = Fixture::new();
    fixture.old_binary();
    fs::write(
        fixture.repo.join("src/main.rs"),
        "compile_error!(\"deliberate fixture failure\"); fn main() {}\n",
    )
    .unwrap();
    fixture.commit(&["src/main.rs"]);
    fixture.rejected(fixture.install(true), "Cargo build failed");
    assert_eq!(
        fs::read(fixture.destination()).unwrap(),
        b"original executable"
    );
}

#[test]
fn repository_escape_paths_fail_before_destination_mutation() {
    for path in ["../outside", "/outside"] {
        let mut fixture = Fixture::new();
        fixture.old_binary();
        fs::write(
            fixture.repo.join(".zpkg.toml"),
            MANIFEST.replace("target/release/fixture-cli", path),
        )
        .unwrap();
        fixture.commit(&[".zpkg.toml"]);
        fixture.rejected(fixture.install(true), "[bin] output");
        assert_eq!(
            fs::read(fixture.destination()).unwrap(),
            b"original executable"
        );
    }
}

#[test]
fn malformed_manifest_fails_closed() {
    let mut fixture = Fixture::new();
    fs::write(fixture.repo.join(".zpkg.toml"), "[broken").unwrap();
    fixture.commit(&[".zpkg.toml"]);
    fixture.rejected(fixture.install(false), "parsing .zpkg.toml");
    assert!(!fixture.destination().exists());
}

#[test]
fn missing_declared_flags_contract_fails_closed() {
    let mut fixture = Fixture::new();
    fs::write(
        fixture.repo.join(".zpkg.toml"),
        format!("{MANIFEST}\n[cli]\nflags_contract = 'missing.toml'\n"),
    )
    .unwrap();
    fixture.commit(&[".zpkg.toml"]);
    fixture.rejected(fixture.install(false), "flags contract is missing");
    assert!(!fixture.destination().exists());
}

#[test]
fn invalid_declared_flags_contract_fails_closed() {
    let mut fixture = Fixture::new();
    fs::write(
        fixture.repo.join(".zpkg.toml"),
        format!("{MANIFEST}\n[cli]\nflags_contract = 'invalid.toml'\n"),
    )
    .unwrap();
    fs::write(fixture.repo.join("invalid.toml"), "[broken").unwrap();
    fixture.commit(&[".zpkg.toml", "invalid.toml"]);
    fixture.rejected(fixture.install(false), "flags2env target contract audit failed");
    assert!(!fixture.destination().exists());
}

#[test]
fn non_force_dangling_destination_is_preserved() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.destination().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("absent-target", fixture.destination()).unwrap();
    fixture.rejected(
        fixture.install(false),
        "without overwriting an existing entry",
    );
    assert_eq!(
        fs::read_link(fixture.destination()).unwrap(),
        Path::new("absent-target")
    );
}

#[test]
fn unknown_options_and_mutable_revisions_fail_closed() {
    let fixture = Fixture::new();
    let output = fixture
        .installer(&fixture.revision)
        .arg("--not-an-installer-option")
        .output()
        .unwrap();
    fixture.rejected(output, "flags2env rejected unknown Git install option(s)");
    let output = fixture.installer("main").output().unwrap();
    fixture.rejected(output, "--rev must be a full");
    assert!(!fixture.destination().exists());
}

#[test]
fn symlinked_repository_manifest_is_rejected() {
    let mut fixture = Fixture::new();
    fs::rename(
        fixture.repo.join(".zpkg.toml"),
        fixture.repo.join("manifest.toml"),
    )
    .unwrap();
    std::os::unix::fs::symlink("manifest.toml", fixture.repo.join(".zpkg.toml")).unwrap();
    fixture.commit(&[".zpkg.toml", "manifest.toml"]);
    fixture.rejected(
        fixture.install(false),
        "must be a regular repository-owned file",
    );
    assert!(!fixture.destination().exists());
}

#[test]
fn force_replaces_symlink_entry_without_modifying_target() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.destination().parent().unwrap()).unwrap();
    let target = fixture.root.path().join("unrelated-file");
    fs::write(&target, b"unrelated user data").unwrap();
    std::os::unix::fs::symlink(&target, fixture.destination()).unwrap();
    successful(fixture.install(true));
    assert!(
        !fs::symlink_metadata(fixture.destination())
            .unwrap()
            .is_symlink()
    );
    assert_eq!(fs::read(target).unwrap(), b"unrelated user data");
    let output = successful(Command::new(fixture.destination()).output().unwrap());
    assert_eq!(output.stdout, b"first\n");
}
