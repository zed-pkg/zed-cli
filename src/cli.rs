use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Every flag can also be set through a `ZED_PKG_*` environment variable,
/// following the flags-2-env convention (github.com/oresoftware/flags-2-env).
#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
pub struct Cli {
    #[command(flatten)]
    pub globals: Globals,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Args)]
pub struct Globals {
    /// Registry base URL (https://... or file:///... for a local registry)
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_REGISTRY",
        default_value = zed_interfaces::registry::DEFAULT_REGISTRY_URL
    )]
    pub registry: String,

    /// zed home directory (store, cache, credentials); defaults to ~/.zed-pkg
    #[arg(long, global = true, env = "ZED_PKG_HOME")]
    pub home: Option<PathBuf>,

    /// Registry auth token; overrides saved credentials
    #[arg(long, global = true, env = "ZED_PKG_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InstallMode {
    /// Symlink from the global store into zed_modules/ (pnpm-style)
    Symlink,
    /// Copy files out of the store; use inside container image builds so
    /// layers stay self-contained across multi-stage COPYs
    Copy,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create a .zpkg.toml manifest in the current directory
    Init {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Add a dependency (org/name[@semver-req]) and install it
    Add { spec: String },
    /// Remove a dependency
    Remove { spec: String },
    /// Resolve and install dependencies into zed_modules/
    #[command(alias = "i")]
    Install {
        /// Install exactly what .zpkg.lock pins; fail on any drift
        #[arg(long, env = "ZED_PKG_FROZEN")]
        frozen: bool,
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_INSTALL_MODE",
            default_value = "symlink"
        )]
        install_mode: InstallMode,
    },
    /// Search the registry
    Find { query: String },
    /// Build the pruned, deterministic artifact for this package
    Pack {
        #[arg(long, env = "ZED_PKG_PACK_OUT")]
        out: Option<PathBuf>,
    },
    /// Pack, verify VCS tag provenance, and upload to the registry
    Publish {
        #[arg(long)]
        dry_run: bool,
        /// Skip the clean-worktree check
        #[arg(long, env = "ZED_PKG_ALLOW_DIRTY")]
        allow_dirty: bool,
        /// Skip tag/commit verification (loud warning; for VCS systems
        /// zed cannot verify yet)
        #[arg(long, env = "ZED_PKG_SKIP_VCS_CHECKS")]
        skip_vcs_checks: bool,
    },
    /// Test this package the way a consumer would install it (r2g-style)
    #[command(name = "test-local", alias = "r2g")]
    TestLocal,
    /// Save a registry token to ~/.zed-pkg/credentials.toml
    Login,
    /// Org namespace operations
    Org {
        #[command(subcommand)]
        cmd: OrgCmd,
    },
    /// Global store operations
    Store {
        #[command(subcommand)]
        cmd: StoreCmd,
    },
    /// Download cache operations
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum OrgCmd {
    /// Claim an org namespace on the registry
    Claim { slug: String },
}

#[derive(Debug, Subcommand)]
pub enum StoreCmd {
    /// Show package count and disk usage
    Status,
    /// Print the store root path
    Path,
    /// Remove store entries no known project references
    Prune,
}

#[derive(Debug, Subcommand)]
pub enum CacheCmd {
    /// Delete all cached artifact downloads
    Clean,
}
