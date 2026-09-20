//! Built-in admission for repository-owned `contracts/` and `conformance/` roots.
//!
//! Zed treats these roots as a paired trust boundary. Repositories that do not
//! use either root remain unaffected. Once either root is present, both must be
//! present as real directories, their trees must be free of symlinks, contracts
//! must contain at least one real file, and conformance must expose a supported
//! executable checker. Lifecycle phases can request structural-only admission
//! (before dependency materialization) or executable admission.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use walkdir::WalkDir;

const CONTRACTS_ROOT: &str = "contracts";
const CONFORMANCE_ROOT: &str = "conformance";
const EXECUTION_STACK_ENV: &str = "ZED_PKG_BOUNDARY_STACK";

// Ordered by fleet preference. Exactly one checker is selected so repositories
// may carry compatibility helpers without Zed accidentally running several
// independent programs with different platform/tool requirements.
const CHECKERS: &[&str] = &[
    "conformance/check.mjs",
    "conformance/check.js",
    "conformance/check.sh",
    "conformance/check.ps1",
    "conformance/check.cmd",
    "conformance/check",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryMode {
    Structural,
    Execute,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BoundaryReport {
    pub enabled: bool,
    pub contracts_file_count: usize,
    pub conformance_file_count: usize,
    pub selected_checker: Option<String>,
    pub checker_executed: bool,
}

impl BoundaryReport {
    fn disabled() -> Self {
        Self {
            enabled: false,
            contracts_file_count: 0,
            conformance_file_count: 0,
            selected_checker: None,
            checker_executed: false,
        }
    }
}

pub fn check(project: &Path, mode: BoundaryMode) -> Result<BoundaryReport> {
    let contracts = project.join(CONTRACTS_ROOT);
    let conformance = project.join(CONFORMANCE_ROOT);
    let contracts_present = path_present(&contracts)?;
    let conformance_present = path_present(&conformance)?;

    if !contracts_present && !conformance_present {
        return Ok(BoundaryReport::disabled());
    }

    ensure!(
        contracts_present && conformance_present,
        "repository boundary is incomplete: `{CONTRACTS_ROOT}/` and `{CONFORMANCE_ROOT}/` must be present together"
    );

    require_real_directory(&contracts, CONTRACTS_ROOT)?;
    require_real_directory(&conformance, CONFORMANCE_ROOT)?;

    let contracts_file_count = inspect_tree(&contracts, CONTRACTS_ROOT)?;
    let conformance_file_count = inspect_tree(&conformance, CONFORMANCE_ROOT)?;
    ensure!(
        contracts_file_count > 0,
        "`{CONTRACTS_ROOT}/` is present but contains no regular contract files"
    );
    ensure!(
        conformance_file_count > 0,
        "`{CONFORMANCE_ROOT}/` is present but contains no regular conformance files"
    );

    let checker = discover_checker(project)?.ok_or_else(|| {
        anyhow::anyhow!(
            "`{CONFORMANCE_ROOT}/` has no supported checker; add one of: {}",
            CHECKERS.join(", ")
        )
    })?;
    let selected_checker = render_relative(&checker);

    let nested = env::var(EXECUTION_STACK_ENV)
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let checker_executed = mode == BoundaryMode::Execute && !nested;
    if checker_executed {
        execute_checker(project, &checker)?;
    }

    Ok(BoundaryReport {
        enabled: true,
        contracts_file_count,
        conformance_file_count,
        selected_checker: Some(selected_checker),
        checker_executed,
    })
}

pub fn run_cli(project: &Path, structural_only: bool, json: bool) -> Result<()> {
    let mode = if structural_only {
        BoundaryMode::Structural
    } else {
        BoundaryMode::Execute
    };
    let report = check(project, mode)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if !report.enabled {
        println!("project boundary: not configured (no contracts/ or conformance/ root)");
    } else {
        println!(
            "project boundary: ok ({} contract files, {} conformance files, checker {}, executed={})",
            report.contracts_file_count,
            report.conformance_file_count,
            report.selected_checker.as_deref().unwrap_or("none"),
            report.checker_executed
        );
    }
    Ok(())
}

fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading repository boundary {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "`{label}/` must not be a symbolic link"
    );
    ensure!(metadata.is_dir(), "`{label}/` must be a directory");
    Ok(())
}

fn inspect_tree(root: &Path, label: &str) -> Result<usize> {
    let mut files = 0usize;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.with_context(|| format!("walking `{label}/`"))?;
        let file_type = entry.file_type();
        ensure!(
            !file_type.is_symlink(),
            "`{label}/` contains forbidden symlink {}",
            entry.path().display()
        );
        if file_type.is_file() {
            files += 1;
        }
    }
    Ok(files)
}

fn discover_checker(project: &Path) -> Result<Option<PathBuf>> {
    for relative in CHECKERS {
        let path = project.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink() && metadata.is_file(),
                    "conformance checker `{relative}` must be a regular non-symlink file"
                );
                return Ok(Some(PathBuf::from(relative)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading checker {}", path.display()));
            }
        }
    }
    Ok(None)
}

fn execute_checker(project: &Path, checker: &Path) -> Result<()> {
    let rendered = render_relative(checker);
    if rendered.ends_with(".mjs") || rendered.ends_with(".js") {
        run_command(project, "node", &["--check", &rendered], &rendered)?;
        return run_command(project, "node", &[&rendered], &rendered);
    }
    if rendered.ends_with(".sh") {
        return run_command(project, "sh", &[&rendered], &rendered);
    }
    if rendered.ends_with(".ps1") {
        let shell = if cfg!(windows) { "powershell" } else { "pwsh" };
        return run_command(
            project,
            shell,
            &["-NoProfile", "-NonInteractive", "-File", &rendered],
            &rendered,
        );
    }
    if rendered.ends_with(".cmd") {
        return run_command(project, "cmd", &["/D", "/C", &rendered], &rendered);
    }

    let executable = project.join(checker);
    let executable = executable.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "conformance checker path is not valid UTF-8: {}",
            executable.display()
        )
    })?;
    run_command(project, executable, &[], &rendered)
}

fn run_command(project: &Path, program: &str, args: &[&str], label: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .current_dir(project)
        .env(EXECUTION_STACK_ENV, "1")
        .status()
        .with_context(|| format!("starting conformance checker `{label}` with `{program}`"))?;
    if !status.success() {
        bail!("conformance checker `{label}` failed with status {status}");
    }
    Ok(())
}

fn render_relative(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{BoundaryMode, check};

    #[test]
    fn absent_boundary_is_opt_in() {
        let dir = tempdir().unwrap();
        let report = check(dir.path(), BoundaryMode::Structural).unwrap();
        assert!(!report.enabled);
    }

    #[test]
    fn paired_roots_are_required() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("contracts")).unwrap();
        let error = check(dir.path(), BoundaryMode::Structural).unwrap_err();
        assert!(error.to_string().contains("must be present together"));
    }

    #[test]
    fn checker_is_required_when_boundary_is_enabled() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("contracts")).unwrap();
        fs::write(dir.path().join("contracts/schema.json"), "{}\n").unwrap();
        fs::create_dir(dir.path().join("conformance")).unwrap();
        fs::write(dir.path().join("conformance/README.md"), "# cases\n").unwrap();
        let error = check(dir.path(), BoundaryMode::Structural).unwrap_err();
        assert!(error.to_string().contains("no supported checker"));
    }

    #[test]
    fn structural_admission_selects_canonical_checker() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("contracts")).unwrap();
        fs::write(dir.path().join("contracts/schema.json"), "{}\n").unwrap();
        fs::create_dir(dir.path().join("conformance")).unwrap();
        fs::write(
            dir.path().join("conformance/check.mjs"),
            "console.log('ok');\n",
        )
        .unwrap();
        fs::write(dir.path().join("conformance/check.sh"), "exit 99\n").unwrap();
        let report = check(dir.path(), BoundaryMode::Structural).unwrap();
        assert!(report.enabled);
        assert_eq!(report.contracts_file_count, 1);
        assert_eq!(report.conformance_file_count, 2);
        assert_eq!(
            report.selected_checker.as_deref(),
            Some("conformance/check.mjs")
        );
        assert!(!report.checker_executed);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_fail_closed() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("contracts")).unwrap();
        fs::write(dir.path().join("real.json"), "{}\n").unwrap();
        symlink(
            dir.path().join("real.json"),
            dir.path().join("contracts/schema.json"),
        )
        .unwrap();
        fs::create_dir(dir.path().join("conformance")).unwrap();
        fs::write(
            dir.path().join("conformance/check.mjs"),
            "console.log('ok');\n",
        )
        .unwrap();
        let error = check(dir.path(), BoundaryMode::Structural).unwrap_err();
        assert!(error.to_string().contains("forbidden symlink"));
    }
}
