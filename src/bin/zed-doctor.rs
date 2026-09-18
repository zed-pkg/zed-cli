use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "zed-doctor", about = "Read-only Zed diagnostics")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect local lock storage without acquiring, deleting, or rewriting locks.
    Locks {
        /// Emit stable machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Serialize)]
struct LockReport {
    schema: &'static str,
    state: &'static str,
    safe: bool,
    lock_root: String,
    ownership: &'static str,
    findings: Vec<Finding>,
}

#[derive(Debug, Serialize)]
struct Finding {
    name: String,
    kind: &'static str,
    severity: &'static str,
    detail: String,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Locks { json } => {
            let home = configured_home()?;
            let report = inspect_lock_root(&home)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_human(&report);
            }
            if !report.safe {
                bail!("unsafe local lock storage; no files were changed")
            }
            Ok(())
        }
    }
}

fn configured_home() -> Result<PathBuf> {
    if let Some(value) = env::var_os("ZED_PKG_HOME")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let home = dirs::home_dir().context("cannot determine user home for Zed lock diagnostics")?;
    Ok(home.join(".zed-pkg"))
}

fn inspect_lock_root(home: &Path) -> Result<LockReport> {
    let lock_root = home.join("locks");
    let mut findings = Vec::new();
    let metadata = match fs::symlink_metadata(&lock_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(LockReport {
                schema: "zed.doctor.locks.v1",
                state: "not_initialized",
                safe: true,
                lock_root: lock_root.display().to_string(),
                ownership: "not_inferred_from_lock_file_contents",
                findings,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", lock_root.display()));
        }
    };

    let mut safe = true;
    if metadata.file_type().is_symlink() {
        safe = false;
        findings.push(finding(
            ".",
            "lock_root_symlink",
            "error",
            "lock root is a symlink; descriptor/handle locks require a non-substitutable private directory",
        ));
    } else if !metadata.is_dir() {
        safe = false;
        findings.push(finding(
            ".",
            "lock_root_not_directory",
            "error",
            "lock root exists but is not a directory",
        ));
    } else {
        safe &= inspect_root_permissions(&metadata, &mut findings);
        safe &= inspect_entries(&lock_root, &mut findings)?;
    }

    Ok(LockReport {
        schema: "zed.doctor.locks.v1",
        state: if safe { "safe" } else { "unsafe" },
        safe,
        lock_root: lock_root.display().to_string(),
        ownership: "not_inferred_from_lock_file_contents",
        findings,
    })
}

fn inspect_entries(lock_root: &Path, findings: &mut Vec<Finding>) -> Result<bool> {
    let mut entries = fs::read_dir(lock_root)
        .with_context(|| format!("read lock root {}", lock_root.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut safe = true;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            safe = false;
            findings.push(finding(
                &name,
                "symlink",
                "error",
                "symlinked lock artifacts are unsafe and are not followed",
            ));
        } else if metadata.is_file() && name.ends_with(".lock") {
            safe &= inspect_lock_file_permissions(&name, &metadata, findings);
        } else if metadata.is_file() || metadata.is_dir() {
            findings.push(finding(
                &name,
                "unknown_preserved",
                "info",
                "unknown legacy artifact preserved; doctor is read-only",
            ));
        } else {
            safe = false;
            findings.push(finding(
                &name,
                "special_file",
                "error",
                "special filesystem objects are not valid lock rendezvous artifacts",
            ));
        }
    }
    Ok(safe)
}

#[cfg(unix)]
fn inspect_root_permissions(metadata: &fs::Metadata, findings: &mut Vec<Finding>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if mode == 0o700 {
        true
    } else {
        findings.push(finding(
            ".",
            "lock_root_permissions",
            "error",
            &format!("expected mode 0700, found {mode:04o}"),
        ));
        false
    }
}

#[cfg(not(unix))]
fn inspect_root_permissions(_metadata: &fs::Metadata, findings: &mut Vec<Finding>) -> bool {
    findings.push(finding(
        ".",
        "lock_root_permissions",
        "info",
        "POSIX permission bits do not apply on this platform",
    ));
    true
}

#[cfg(unix)]
fn inspect_lock_file_permissions(
    name: &str,
    metadata: &fs::Metadata,
    findings: &mut Vec<Finding>,
) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if mode == 0o600 {
        findings.push(finding(
            name,
            "lock_file",
            "info",
            "private 0600 rendezvous file",
        ));
        true
    } else {
        findings.push(finding(
            name,
            "lock_file_permissions",
            "error",
            &format!("expected mode 0600, found {mode:04o}"),
        ));
        false
    }
}

#[cfg(not(unix))]
fn inspect_lock_file_permissions(
    name: &str,
    _metadata: &fs::Metadata,
    findings: &mut Vec<Finding>,
) -> bool {
    findings.push(finding(
        name,
        "lock_file",
        "info",
        "regular rendezvous file; live ownership is held by the native handle",
    ));
    true
}

fn finding(name: &str, kind: &'static str, severity: &'static str, detail: &str) -> Finding {
    Finding {
        name: name.to_owned(),
        kind,
        severity,
        detail: detail.to_owned(),
    }
}

fn print_human(report: &LockReport) {
    println!("lock-root: {}", report.lock_root);
    println!("state: {}", report.state);
    println!("ownership: {}", report.ownership);
    for finding in &report.findings {
        println!(
            "{} {} {}: {}",
            finding.severity, finding.kind, finding.name, finding.detail
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_lock_root_is_safe_and_not_initialized() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let report = inspect_lock_root(&temp.path().join("home"))?;
        assert!(report.safe);
        assert_eq!(report.state, "not_initialized");
        assert_eq!(report.ownership, "not_inferred_from_lock_file_contents");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_root_and_regular_lock_are_safe() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let lock_root = temp.path().join("home/locks");
        fs::create_dir_all(&lock_root)?;
        fs::set_permissions(&lock_root, fs::Permissions::from_mode(0o700))?;
        let lock = lock_root.join("install.lock");
        fs::write(&lock, b"")?;
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600))?;

        let report = inspect_lock_root(&temp.path().join("home"))?;
        assert!(report.safe, "{report:?}");
        assert_eq!(report.state, "safe");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn weak_root_permissions_fail_closed_without_mutation() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let lock_root = temp.path().join("home/locks");
        fs::create_dir_all(&lock_root)?;
        fs::set_permissions(&lock_root, fs::Permissions::from_mode(0o755))?;

        let report = inspect_lock_root(&temp.path().join("home"))?;
        assert!(!report.safe);
        assert_eq!(report.state, "unsafe");
        assert_eq!(
            fs::metadata(&lock_root)?.permissions().mode() & 0o777,
            0o755
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_lock_root_is_rejected_without_following_it() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let target = temp.path().join("target");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&target)?;
        symlink(&target, home.join("locks"))?;

        let report = inspect_lock_root(&home)?;
        assert!(!report.safe);
        assert!(target.is_dir());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unknown_legacy_artifact_is_reported_and_preserved() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let lock_root = temp.path().join("home/locks");
        fs::create_dir_all(&lock_root)?;
        fs::set_permissions(&lock_root, fs::Permissions::from_mode(0o700))?;
        let legacy = lock_root.join("legacy-owner-record");
        fs::write(&legacy, b"legacy")?;

        let report = inspect_lock_root(&temp.path().join("home"))?;
        assert!(report.safe, "{report:?}");
        assert!(legacy.exists());
        assert!(
            report
                .findings
                .iter()
                .any(|item| item.kind == "unknown_preserved")
        );
        Ok(())
    }
}
