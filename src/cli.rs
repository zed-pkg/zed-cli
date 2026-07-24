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

/// Contextual adapters translate zed's universal layout into what a
/// language's toolchain expects, per the "structural translation" goal:
/// the same artifact lands where Node, the JVM, or plain zed_modules/
/// consumers respectively look for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Adapter {
    /// Detect from the project: package.json -> node, pom.xml/build.gradle
    /// -> java, otherwise none
    Auto,
    /// zed_modules/ only
    None,
    /// Additionally link into node_modules/@<org>/<name> for Node resolution
    Node,
    /// Additionally write .zed/classpath listing installed .jar paths for
    /// javac/java -cp and build-tool integration
    Java,
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
        /// Also link packages where the language ecosystem expects them,
        /// inferred from the project by default (experimental; python
        /// site-packages and deeper maven integration are planned)
        #[arg(long, value_enum, env = "ZED_PKG_ADAPTER", default_value = "auto")]
        adapter: Adapter,
        /// Run dependencies' [build] commands (arbitrary code from the
        /// package author — off by default; builds are cached per
        /// (artifact, platform) under ~/.zed-pkg/builds)
        #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]
        allow_build: bool,
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
    /// Run an executable exposed by an installed package (hoisted into
    /// zed_modules/.bin), with that directory prepended to PATH
    Run {
        /// Name of the binary as declared in the package's [bin] table
        bin: String,
        /// Arguments passed through to the binary
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Mark a published version as yanked (hidden from resolution; still
    /// downloadable for existing lockfiles). --undo restores it.
    Yank {
        /// org/name@version
        spec: String,
        #[arg(long)]
        undo: bool,
    },
    /// Garbage-collect the store: remove entries unreferenced by any live
    /// project and unused past the age cutoff, plus stale download caches
    Gc {
        /// Only collect entries unused for at least this many days
        #[arg(long, env = "ZED_PKG_GC_MAX_AGE_DAYS", default_value_t = 90)]
        max_age_days: u64,
    },
    /// Update zed itself to the latest GitHub release
    #[command(name = "self-update")]
    SelfUpdate {
        /// Check and report, but do not replace the binary
        #[arg(long)]
        check: bool,
    },
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    /// The flags-2-env convention (github.com/oresoftware/flags-2-env):
    /// every user-facing option must be settable via a ZED_PKG_* env var.
    #[test]
    fn flags_2_env_convention_holds() {
        let cmd = Cli::command();
        for arg in cmd.get_arguments() {
            let Some(long) = arg.get_long() else { continue };
            if long == "help" || long == "version" {
                continue;
            }
            let env = arg
                .get_env()
                .unwrap_or_else(|| panic!("--{long} lacks an env fallback"))
                .to_string_lossy();
            assert!(
                env.starts_with("ZED_PKG_"),
                "--{long} env `{env}` must use the ZED_PKG_ prefix"
            );
        }

        let env_of = |name: &str| {
            Cli::command()
                .get_arguments()
                .find(|a| a.get_long() == Some(name))
                .and_then(|a| a.get_env().map(|e| e.to_string_lossy().to_string()))
        };
        assert_eq!(env_of("registry").as_deref(), Some("ZED_PKG_REGISTRY"));
        assert_eq!(env_of("home").as_deref(), Some("ZED_PKG_HOME"));
        assert_eq!(env_of("token").as_deref(), Some("ZED_PKG_TOKEN"));
    }
}
