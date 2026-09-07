//! Cargo-style source installation for repository-owned CLI binaries.
//!
//! This executable is shipped beside `zed`, so the ordinary external-command
//! dispatcher exposes it as `zed git-install`. It intentionally starts with a
//! narrow, auditable contract equivalent to Cargo's immutable Git install path:
//! `--git`, `--rev`, `--bin`, and `--force`.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use flags2env::BundledFlags2Env;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Parser)]
#[command(
    name = "zed-git-install",
    version,
    about = "Install a revision-pinned repository CLI into the Zed global bin directory"
)]
struct Cli {
    /// Git repository URL. HTTPS and SSH transports are accepted.
    #[arg(long, env = "ZED_PKG_GIT_INSTALL_URL", value_name = "URL")]
    git: String,

    /// Full immutable Git commit id (40-char SHA-1 or 64-char SHA-256).
    #[arg(long, env = "ZED_PKG_GIT_INSTALL_REV", value_name = "COMMIT")]
    rev: String,

    /// Executable name declared by the repository's `.zpkg.toml` `[bin]` table.
    #[arg(long, env = "ZED_PKG_GIT_INSTALL_BIN", value_name = "NAME")]
    bin: String,

    /// Replace an existing managed executable after the new binary validates.
    #[arg(long, env = "ZED_PKG_GIT_INSTALL_FORCE")]
    force: bool,

    /// Override the user directory placed on PATH for global Zed executables.
    #[arg(long, env = "ZED_PKG_GLOBAL_BIN_DIR", value_name = "PATH")]
    global_bin_dir: Option<PathBuf>,

    /// Override Zed's state directory. Defaults to `~/.zed-pkg`.
    #[arg(long, env = "ZED_PKG_HOME", value_name = "PATH")]
    home: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    source: &'a str,
    revision: &'a str,
    binary: &'a str,
    installed_path: String,
    sha256: String,
    manifest: String,
    flags_contract: Option<String>,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    validate_url(&cli.git)?;
    validate_revision(&cli.rev)?;
    validate_bin_name(&cli.bin)?;

    let checkout = tempfile::tempdir().context("creating temporary Git checkout")?;
    let repo = checkout.path().join("repo");
    fs::create_dir(&repo).context("creating checkout directory")?;

    run_command(
        Command::new("git").arg("init").arg("--quiet").arg(&repo),
        "initializing Git checkout",
    )?;
    run_command(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(&cli.git),
        "configuring Git source",
    )?;
    run_command(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("fetch")
            .arg("--quiet")
            .arg("--depth=1")
            .arg("origin")
            .arg(&cli.rev),
        "fetching pinned Git revision",
    )?;
    run_command(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("checkout")
            .arg("--quiet")
            .arg("--detach")
            .arg("FETCH_HEAD"),
        "checking out pinned Git revision",
    )?;

    let actual_rev = command_stdout(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("rev-parse")
            .arg("HEAD"),
        "resolving checked-out Git revision",
    )?;
    ensure!(
        actual_rev.eq_ignore_ascii_case(&cli.rev),
        "Git source resolved to `{actual_rev}` instead of requested immutable revision `{}`",
        cli.rev
    );

    let manifest_path = repo.join(".zpkg.toml");
    let manifest_metadata = fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("source repository is missing {}", manifest_path.display()))?;
    ensure!(
        manifest_metadata.is_file() && !manifest_metadata.file_type().is_symlink(),
        "`.zpkg.toml` must be a regular repository-owned file"
    );
    let manifest_text = fs::read_to_string(&manifest_path).context("reading .zpkg.toml")?;
    let manifest: toml::Value = toml::from_str(&manifest_text).context("parsing .zpkg.toml")?;

    let output = declared_bin(&manifest, &cli.bin)?;
    let output = safe_relative_path(output, "[bin] output")?;
    let flags_contract = audit_flags_contract(&repo, &manifest)?;

    let language = manifest
        .get("package")
        .and_then(|package| package.get("language"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    ensure!(
        language.eq_ignore_ascii_case("rust"),
        "revision-pinned Git install currently supports Rust source packages; `.zpkg.toml` declares language `{language}`"
    );

    let status = Command::new("cargo")
        .current_dir(&repo)
        .env("CARGO_NET_GIT_FETCH_WITH_CLI", "true")
        .arg("build")
        .arg("--release")
        .arg("--locked")
        .arg("--bin")
        .arg(&cli.bin)
        .status()
        .context("starting Cargo build")?;
    ensure!(status.success(), "Cargo build failed with {status}");

    let built = repo.join(&output);
    let built_metadata = fs::symlink_metadata(&built).with_context(|| {
        format!(
            "declared binary output was not produced: {}",
            built.display()
        )
    })?;
    ensure!(
        built_metadata.is_file() && !built_metadata.file_type().is_symlink(),
        "declared binary output must be a regular file: {}",
        built.display()
    );

    let bin_dir = resolve_bin_dir(cli.global_bin_dir.as_deref())?;
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("creating global bin directory {}", bin_dir.display()))?;
    let destination = bin_dir.join(platform_bin_name(&cli.bin));
    install_atomically(&built, &destination, cli.force)?;

    let sha256 = sha256_file(&destination)?;
    let home = resolve_home(cli.home.as_deref())?;
    let receipts = home.join("global").join("git-installs");
    fs::create_dir_all(&receipts).context("creating Git install receipt directory")?;
    let receipt_path = receipts.join(format!("{}-{}.json", cli.bin, &cli.rev[..12]));
    let receipt = Receipt {
        schema_version: 1,
        source: &cli.git,
        revision: &actual_rev,
        binary: &cli.bin,
        installed_path: destination.display().to_string(),
        sha256,
        manifest: ".zpkg.toml".to_string(),
        flags_contract: flags_contract.map(|path| path.display().to_string()),
    };
    let encoded = serde_json::to_vec_pretty(&receipt).context("encoding install receipt")?;
    write_atomic(&receipt_path, &encoded)?;

    println!("installed {} from {}@{}", cli.bin, cli.git, actual_rev);
    println!("bin: {}", destination.display());
    println!("receipt: {}", receipt_path.display());
    print_path_guidance(&bin_dir);
    Ok(())
}

fn validate_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    ensure!(!trimmed.is_empty(), "--git URL cannot be empty");
    ensure!(!trimmed.starts_with('-'), "--git URL cannot start with '-'");
    ensure!(
        trimmed.starts_with("https://")
            || trimmed.starts_with("ssh://")
            || trimmed.starts_with("git@"),
        "--git must use an https://, ssh://, or git@ SSH source"
    );
    if let Some(authority) = trimmed
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
    {
        ensure!(
            !authority.contains('@'),
            "credentials must not be embedded in --git URLs"
        );
    }
    Ok(())
}

fn validate_revision(rev: &str) -> Result<()> {
    ensure!(
        matches!(rev.len(), 40 | 64) && rev.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "--rev must be a full 40-character SHA-1 or 64-character SHA-256 commit id"
    );
    Ok(())
}

fn validate_bin_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 128, "invalid --bin name");
    ensure!(
        name.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "--bin may contain only ASCII letters, digits, '-' and '_'"
    );
    Ok(())
}

fn declared_bin<'a>(manifest: &'a toml::Value, name: &str) -> Result<&'a str> {
    let bins = manifest
        .get("bin")
        .and_then(toml::Value::as_table)
        .context(".zpkg.toml must declare a [bin] table for Git CLI installation")?;
    bins.get(name)
        .and_then(toml::Value::as_str)
        .with_context(|| format!("binary `{name}` is not declared in .zpkg.toml [bin]"))
}

fn safe_relative_path(value: &str, label: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    ensure!(!path.is_absolute(), "{label} must be repository-relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir)),
        "{label} must not escape the source repository"
    );
    Ok(path.to_path_buf())
}

fn audit_flags_contract(repo: &Path, manifest: &toml::Value) -> Result<Option<PathBuf>> {
    let declared = manifest
        .get("cli")
        .and_then(|cli| cli.get("flags_contract"))
        .and_then(toml::Value::as_str);
    let fallback = repo.join(".cli-flags.toml");
    let path = match declared {
        Some(path) => repo.join(safe_relative_path(path, "[cli].flags_contract")?),
        None if fallback.exists() => fallback,
        None => return Ok(None),
    };
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("declared flags contract is missing: {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "flags contract must be a regular repository-owned file"
    );
    let path_str = path.to_str().context("flags contract path is not UTF-8")?;
    BundledFlags2Env::new()
        .audit_config(Some(path_str))
        .map_err(|error| anyhow::anyhow!("flags2env contract audit failed: {error}"))?;
    Ok(Some(path))
}

fn run_command(command: &mut Command, action: &str) -> Result<()> {
    let status = command
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("{action}: failed to start process"))?;
    ensure!(status.success(), "{action}: process exited with {status}");
    Ok(())
}

fn command_stdout(command: &mut Command, action: &str) -> Result<String> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("{action}: failed to start process"))?;
    ensure!(
        output.status.success(),
        "{action}: process exited with {}",
        output.status
    );
    let text = String::from_utf8(output.stdout)
        .with_context(|| format!("{action}: stdout was not UTF-8"))?;
    Ok(text.trim().to_string())
}

fn resolve_home(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    dirs::home_dir()
        .map(|home| home.join(".zed-pkg"))
        .context("cannot resolve home directory; pass --home")
}

#[cfg(windows)]
fn resolve_bin_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    dirs::data_local_dir()
        .map(|root| root.join("zed-pkg").join("bin"))
        .context("cannot resolve local application data directory; pass --global-bin-dir")
}

#[cfg(not(windows))]
fn resolve_bin_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    dirs::home_dir()
        .map(|home| home.join(".local").join("bin"))
        .context("cannot resolve home directory; pass --global-bin-dir")
}

#[cfg(windows)]
fn platform_bin_name(name: &str) -> String {
    format!("{name}.exe")
}

#[cfg(not(windows))]
fn platform_bin_name(name: &str) -> String {
    name.to_string()
}

fn install_atomically(source: &Path, destination: &Path, force: bool) -> Result<()> {
    if destination.exists() && !force {
        bail!(
            "{} already exists; pass --force to replace it after validation",
            destination.display()
        );
    }
    let parent = destination
        .parent()
        .context("global binary destination has no parent")?;
    let mut staged =
        tempfile::NamedTempFile::new_in(parent).context("staging global executable")?;
    let mut input = fs::File::open(source).context("opening built executable")?;
    std::io::copy(&mut input, staged.as_file_mut()).context("copying built executable")?;
    staged
        .as_file_mut()
        .sync_all()
        .context("syncing staged executable")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = staged.as_file().metadata()?.permissions();
        permissions.set_mode(0o755);
        staged.as_file().set_permissions(permissions)?;
    }

    let temporary = staged.into_temp_path();
    let backup = destination.with_extension(format!("zed-backup-{}", std::process::id()));
    if destination.exists() {
        if backup.exists() {
            fs::remove_file(&backup).context("removing stale executable backup")?;
        }
        fs::rename(destination, &backup).context("staging existing executable backup")?;
    }
    match temporary.persist(destination) {
        Ok(()) => {
            if backup.exists() {
                fs::remove_file(&backup).context("removing replaced executable backup")?;
            }
            Ok(())
        }
        Err(error) => {
            if backup.exists() {
                let _ = fs::rename(&backup, destination);
            }
            Err(error).context("atomically activating global executable")
        }
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        fs::read(path).with_context(|| format!("reading {} for SHA-256", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("receipt path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context("staging receipt")?;
    temporary.write_all(bytes).context("writing receipt")?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("syncing receipt")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("activating receipt")?;
    Ok(())
}

fn print_path_guidance(bin_dir: &Path) {
    let already_present = std::env::var_os("PATH")
        .is_some_and(|value| std::env::split_paths(&value).any(|entry| entry == bin_dir));
    if already_present {
        println!("PATH already contains {}", bin_dir.display());
    } else {
        println!("add {} to PATH", bin_dir.display());
        #[cfg(not(windows))]
        println!("export PATH=\"{}:$PATH\"", bin_dir.display());
    }
}
