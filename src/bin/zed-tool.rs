use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use zed_cli::tool_profile::{
    LockMode, default_zed_home, install_offline, list_target, load_environment_lock, verify_receipt,
};

#[derive(Debug, Parser)]
#[command(
    name = "zed-tool",
    version,
    about = "Verify and replay exact EnvironmentLock tools without a manager or network"
)]
struct Cli {
    /// Project-local EnvironmentLock TOML or JSON.
    #[arg(
        long,
        env = "ZED_TOOL_LOCK",
        default_value = ".zed/environment.lock.toml"
    )]
    lock: PathBuf,

    /// Emit one stable JSON document.
    #[arg(long, env = "ZED_TOOL_JSON")]
    json: bool,

    #[command(subcommand)]
    command: ToolCommand,
}

#[derive(Debug, Subcommand)]
enum ToolCommand {
    /// Validate exact lock structure, portability, and optional plan identity.
    Verify {
        /// Require a machine-independent portable lock.
        #[arg(long, env = "ZED_TOOL_PORTABLE")]
        portable: bool,

        /// Require the lock to belong to this normalized EnvironmentPlan SHA-256.
        #[arg(long, env = "ZED_TOOL_PLAN_DIGEST")]
        plan_digest: Option<String>,
    },

    /// List the exact tool variants selected for one target.
    List {
        /// Exact locked target identity, such as x86_64-unknown-linux-gnu.
        #[arg(long, env = "ZED_TOOL_TARGET")]
        target: String,
    },

    /// Replay exact cached artifacts into a project-local executable profile.
    Install {
        /// Exact locked target identity, such as x86_64-unknown-linux-gnu.
        #[arg(long, env = "ZED_TOOL_TARGET")]
        target: String,

        /// Installation is intentionally cache-only in this first slice.
        #[arg(long, env = "ZED_TOOL_OFFLINE")]
        offline: bool,

        /// Project-local profile root.
        #[arg(long, env = "ZED_TOOL_PROFILE", default_value = ".zed/tools")]
        profile: PathBuf,

        /// Zed content-addressed store and cache root.
        #[arg(long, env = "ZED_PKG_HOME")]
        home: Option<PathBuf>,
    },
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let root = std::env::current_dir()?;
    match cli.command {
        ToolCommand::Verify {
            portable,
            plan_digest,
        } => {
            let mode = if portable {
                LockMode::Portable
            } else {
                LockMode::Local
            };
            let loaded =
                load_environment_lock(&root, Some(&cli.lock), mode, plan_digest.as_deref())?;
            let receipt = verify_receipt(&loaded, mode);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                println!("verified {}", receipt.lock);
                println!("lock-sha256: {}", receipt.lock_sha256);
                println!("plan-sha256: {}", receipt.plan_digest_sha256);
                println!("validation: {}", receipt.validation);
                println!("tools: {}", receipt.tools);
                println!("variants: {}", receipt.variants);
            }
        }
        ToolCommand::List { target } => {
            let loaded = load_environment_lock(&root, Some(&cli.lock), LockMode::Portable, None)?;
            let tools = list_target(&loaded, &target)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&tools)?);
            } else {
                for tool in tools {
                    println!(
                        "{} {} ({}, {}) [{}]",
                        tool.name,
                        tool.resolved,
                        tool.backend,
                        tool.target,
                        tool.executables.join(", ")
                    );
                }
            }
        }
        ToolCommand::Install {
            target,
            offline,
            profile,
            home,
        } => {
            ensure!(
                offline,
                "native tool installation currently requires `--offline`; version discovery and downloads are not yet certified"
            );
            let loaded = load_environment_lock(&root, Some(&cli.lock), LockMode::Portable, None)?;
            let home = match home {
                Some(home) if home.is_absolute() => home,
                Some(home) => root.join(home),
                None => default_zed_home()?,
            };
            let rollback_directories = absent_profile_directories(&root, &profile);
            let receipt = match install_offline(&root, &loaded, &target, Some(&profile), &home) {
                Ok(receipt) => receipt,
                Err(error) => {
                    rollback_empty_profile_directories(&rollback_directories).with_context(|| {
                        format!(
                            "rolling back empty profile directories after installation failed: {error:#}"
                        )
                    })?;
                    return Err(error);
                }
            };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                println!("{} tool profile {}", receipt.action, receipt.profile);
                println!("bin: {}", receipt.bin);
                println!("lock-sha256: {}", receipt.lock_sha256);
                for tool in receipt.tools {
                    println!(
                        "{} {} ({}) [{}]",
                        tool.name,
                        tool.resolved,
                        tool.backend,
                        tool.executables.join(", ")
                    );
                }
            }
        }
    }
    Ok(())
}

/// Record only profile-directory prefixes that do not exist before installation.
/// Unsafe paths are left to the library validator and produce no rollback list.
fn absent_profile_directories(root: &Path, profile: &Path) -> Vec<PathBuf> {
    if profile.as_os_str().is_empty() || profile.is_absolute() {
        return Vec::new();
    }

    let mut current = root.to_path_buf();
    let mut absent = Vec::new();
    for component in profile.components() {
        let Component::Normal(segment) = component else {
            return Vec::new();
        };
        current.push(segment);
        if matches!(
            fs::symlink_metadata(&current),
            Err(error) if error.kind() == ErrorKind::NotFound
        ) {
            absent.push(current.clone());
        }
    }

    let version = current.join("v1");
    if matches!(
        fs::symlink_metadata(&version),
        Err(error) if error.kind() == ErrorKind::NotFound
    ) {
        absent.push(version);
    }
    absent
}

/// Remove only directories that were absent before this invocation and remain
/// empty. Nonempty directories preserve forensic or concurrent state and are
/// deliberately left alone; symlink or file replacement fails closed.
fn rollback_empty_profile_directories(directories: &[PathBuf]) -> Result<()> {
    for directory in directories.iter().rev() {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspecting rollback directory `{}`", directory.display())
                });
            }
        };
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "new profile path `{}` changed type during rollback",
            directory.display()
        );

        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                let has_entries = fs::read_dir(directory)
                    .with_context(|| {
                        format!("reading rollback directory `{}`", directory.display())
                    })?
                    .next()
                    .transpose()?
                    .is_some();
                if !has_entries {
                    return Err(error).with_context(|| {
                        format!(
                            "removing empty rollback directory `{}`",
                            directory.display()
                        )
                    });
                }
            }
        }
    }
    Ok(())
}
