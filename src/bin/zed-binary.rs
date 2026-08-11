use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use zed_cli::binary_archive::{
    BinaryPackOptions, download_binary_zip, pack_binary_zip, publish_binary_zip, verify_binary_zip,
};
use zed_cli::cli::Globals;
use zed_cli::config::Config;
use zed_interfaces::binary_artifact::BinaryPlatformV1;

#[derive(Debug, Parser)]
#[command(
    name = "zed-binary",
    bin_name = "zed binary",
    version,
    about = "Pack, verify, publish, and download secure native-binary Zed ZIP artifacts"
)]
struct BinaryCli {
    #[command(flatten)]
    globals: Globals,
    #[command(subcommand)]
    command: BinaryCommand,
}

#[derive(Debug, Subcommand)]
enum BinaryCommand {
    /// Build a deterministic ZIP rooted at pkg/ with .zpkg.toml and .zpkg-binary.json.
    Pack {
        #[command(flatten)]
        platform: RequiredPlatformArgs,
        /// Additional project-relative runtime file or directory to include; repeatable.
        #[arg(long = "include", env = "ZED_PKG_BINARY_INCLUDE")]
        includes: Vec<PathBuf>,
        /// Output directory; defaults to target/zed-pack.
        #[arg(long, env = "ZED_PKG_BINARY_OUT")]
        out: Option<PathBuf>,
        /// Optional source commit copied into the generated descriptor.
        #[arg(long, env = "ZED_PKG_BINARY_VCS_COMMIT")]
        vcs_commit: Option<String>,
        /// Emit a stable JSON summary.
        #[arg(long, env = "ZED_PKG_BINARY_JSON")]
        json: bool,
    },
    /// Fully inspect and hash a binary ZIP without extracting or executing it.
    Verify {
        archive: PathBuf,
        /// Require this exact normalized target.
        #[arg(long, env = "ZED_PKG_BINARY_TARGET")]
        target: Option<String>,
        /// Emit a stable JSON summary.
        #[arg(long, env = "ZED_PKG_BINARY_JSON")]
        json: bool,
    },
    /// Verify VCS provenance, build a binary ZIP, and upload it through the registry.
    Publish {
        #[command(flatten)]
        platform: RequiredPlatformArgs,
        /// Additional project-relative runtime file or directory to include; repeatable.
        #[arg(long = "include", env = "ZED_PKG_BINARY_INCLUDE")]
        includes: Vec<PathBuf>,
        /// Output directory; defaults to target/zed-pack.
        #[arg(long, env = "ZED_PKG_BINARY_OUT")]
        out: Option<PathBuf>,
        /// Pack and verify, but do not upload.
        #[arg(long, env = "ZED_PKG_DRY_RUN")]
        dry_run: bool,
        /// Skip the clean-worktree requirement.
        #[arg(long, env = "ZED_PKG_ALLOW_DIRTY")]
        allow_dirty: bool,
        /// Skip VCS tag/commit verification. This is intentionally loud and records no commit.
        #[arg(long, env = "ZED_PKG_SKIP_VCS_CHECKS")]
        skip_vcs_checks: bool,
        /// Emit a stable JSON summary after a successful or dry-run operation.
        #[arg(long, env = "ZED_PKG_BINARY_JSON")]
        json: bool,
    },
    /// Download an exact published ZIP, verify outer and inner integrity, then promote it.
    Download {
        /// Exact package identity: org/name@version.
        spec: String,
        /// Destination ZIP path.
        #[arg(long, env = "ZED_PKG_BINARY_OUT")]
        out: PathBuf,
        /// Require this exact normalized target from .zpkg-binary.json.
        #[arg(long, env = "ZED_PKG_BINARY_TARGET")]
        target: Option<String>,
        /// Emit a stable JSON summary.
        #[arg(long, env = "ZED_PKG_BINARY_JSON")]
        json: bool,
    },
}

#[derive(Debug, Clone, Args)]
struct RequiredPlatformArgs {
    /// Canonical target token, normally a Rust-style target triple.
    #[arg(long, env = "ZED_PKG_BINARY_TARGET")]
    target: String,
    /// Normalized operating-system token (linux, macos, windows, ...).
    #[arg(long, env = "ZED_PKG_BINARY_OS")]
    os: String,
    /// Normalized architecture token (x86_64, aarch64, ...).
    #[arg(long, env = "ZED_PKG_BINARY_ARCH")]
    arch: String,
    /// Optional C-library family (gnu, musl, ...).
    #[arg(long, env = "ZED_PKG_BINARY_LIBC")]
    libc: Option<String>,
    /// Optional ABI token.
    #[arg(long, env = "ZED_PKG_BINARY_ABI")]
    abi: Option<String>,
}

impl RequiredPlatformArgs {
    fn into_platform(self) -> BinaryPlatformV1 {
        BinaryPlatformV1 {
            target: self.target,
            os: self.os,
            arch: self.arch,
            libc: self.libc,
            abi: self.abi,
        }
    }
}

#[derive(Debug, Serialize)]
struct BinarySummary<'a> {
    package: String,
    version: String,
    target: &'a str,
    archive: String,
    sha256: &'a str,
    size: u64,
    files: usize,
    uploaded: bool,
    dry_run: bool,
}

fn main() {
    if let Err(error) = run(BinaryCli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: BinaryCli) -> Result<()> {
    let cwd = std::env::current_dir().context("determining current directory")?;
    let cfg = Config::from_globals(&cli.globals)?;
    match cli.command {
        BinaryCommand::Pack {
            platform,
            includes,
            out,
            vcs_commit,
            json,
        } => {
            let packed = pack_binary_zip(
                &cwd,
                &BinaryPackOptions {
                    platform: platform.into_platform(),
                    includes,
                    out_dir: out,
                    vcs_commit,
                },
            )?;
            print_summary(&packed, false, false, json)
        }
        BinaryCommand::Verify {
            archive,
            target,
            json,
        } => {
            let verified = verify_binary_zip(&archive, None)?;
            if let Some(target) = target {
                ensure!(
                    verified.descriptor.platform.target == target,
                    "binary target mismatch: expected {target}, got {}",
                    verified.descriptor.platform.target
                );
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&BinarySummary {
                        package: verified.manifest.full_name(),
                        version: verified.manifest.package.version.clone(),
                        target: &verified.descriptor.platform.target,
                        archive: archive.display().to_string(),
                        sha256: &verified.sha256,
                        size: verified.size,
                        files: verified.file_count,
                        uploaded: false,
                        dry_run: false,
                    })?
                );
            } else {
                println!(
                    "verified {}@{} for {}\n  {}\n  sha256 {}\n  size {} bytes ({} files)",
                    verified.manifest.full_name(),
                    verified.manifest.package.version,
                    verified.descriptor.platform.target,
                    archive.display(),
                    verified.sha256,
                    verified.size,
                    verified.file_count
                );
            }
            Ok(())
        }
        BinaryCommand::Publish {
            platform,
            includes,
            out,
            dry_run,
            allow_dirty,
            skip_vcs_checks,
            json,
        } => {
            let manifest = zed_cli::config::read_manifest(&cwd)?;
            let vcs_commit = if skip_vcs_checks {
                eprintln!(
                    "warning: --skip-vcs-checks set; binary descriptor will not record a verified commit"
                );
                None
            } else {
                Some(zed_cli::vcs::verify_publish_provenance(
                    manifest.package.repository.vcs,
                    &cwd,
                    &manifest.vcs_tag(),
                    allow_dirty,
                )?)
            };
            let packed = pack_binary_zip(
                &cwd,
                &BinaryPackOptions {
                    platform: platform.into_platform(),
                    includes,
                    out_dir: out,
                    vcs_commit,
                },
            )?;
            let uploaded = publish_binary_zip(&cfg, &packed, dry_run)?.is_some() && !dry_run;
            print_summary(&packed, uploaded, dry_run, json)
        }
        BinaryCommand::Download {
            spec,
            out,
            target,
            json,
        } => {
            let verified = download_binary_zip(&cfg, &spec, &out, target.as_deref())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&BinarySummary {
                        package: verified.manifest.full_name(),
                        version: verified.manifest.package.version.clone(),
                        target: &verified.descriptor.platform.target,
                        archive: out.display().to_string(),
                        sha256: &verified.sha256,
                        size: verified.size,
                        files: verified.file_count,
                        uploaded: false,
                        dry_run: false,
                    })?
                );
            } else {
                println!(
                    "downloaded and verified {}@{} for {}\n  {}\n  sha256 {}\n  size {} bytes",
                    verified.manifest.full_name(),
                    verified.manifest.package.version,
                    verified.descriptor.platform.target,
                    out.display(),
                    verified.sha256,
                    verified.size
                );
            }
            Ok(())
        }
    }
}

fn print_summary(
    packed: &zed_cli::binary_archive::BinaryPackResult,
    uploaded: bool,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&BinarySummary {
                package: packed.manifest.full_name(),
                version: packed.manifest.package.version.clone(),
                target: &packed.descriptor.platform.target,
                archive: packed.packed.path.display().to_string(),
                sha256: &packed.packed.sha256,
                size: packed.packed.size,
                files: packed.packed.file_count,
                uploaded,
                dry_run,
            })?
        );
    } else {
        println!(
            "{} {}@{} for {}\n  {}\n  sha256 {}\n  size {} bytes ({} files)",
            if uploaded {
                "published"
            } else if dry_run {
                "packed and verified (dry run)"
            } else {
                "packed and verified"
            },
            packed.manifest.full_name(),
            packed.manifest.package.version,
            packed.descriptor.platform.target,
            packed.packed.path.display(),
            packed.packed.sha256,
            packed.packed.size,
            packed.packed.file_count
        );
        if !uploaded && !dry_run {
            println!("publish with: zed binary publish --target ... --os ... --arch ...");
        }
    }
    Ok(())
}
