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

/// OCI runtime used by `zed r2g --docker` to roundtrip-test the package
/// inside a throwaway container. Auto-detected when unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ContainerRuntime {
    Docker,
    Podman,
}

impl ContainerRuntime {
    pub fn program(self) -> &'static str {
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create a .zpkg.toml manifest in the current directory
    Init {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: Option<String>,
        #[arg(long, env = "ZED_PKG_NAME")]
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
        /// (artifact, platform, command) under ~/.zed-pkg/builds)
        #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]
        allow_build: bool,
        /// Which language subtree to take from polyglot dependencies (a repo
        /// shipping e.g. node/, python/, go/). Overrides [install].target;
        /// omitted = infer from the project
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
    },
    /// Run (or warm the build cache for) the [build] steps the locked
    /// dependency graph declares (zed-docs issue #5). Running `zed build` is
    /// itself consent to execute package-author build code, like
    /// `install --allow-build`.
    Build {
        /// Rebuild even when the build cache already has an entry
        #[arg(long, env = "ZED_PKG_FORCE")]
        force: bool,
    },
    /// Run an executable a dependency exposes via [bin] (hoisted into
    /// zed_modules/.bin) or any command, with zed_modules/.bin prepended to
    /// PATH — npx-style, no global pollution (zed-docs issue #7)
    Run {
        /// Binary/command name to execute
        command: String,
        /// Arguments passed through to the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Garbage-collect the store, build cache, and downloads by last use,
    /// LRU-style (zed-docs issue #7); store entries still referenced by a
    /// live project are always kept
    Gc {
        /// Remove entries not used within this window (e.g. 90d, 2w, 12h)
        #[arg(long, env = "ZED_PKG_GC_OLDER_THAN", default_value = "90d")]
        older_than: String,
        /// Report what would be removed without deleting anything
        #[arg(long, env = "ZED_PKG_GC_DRY_RUN")]
        dry_run: bool,
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
        #[arg(long, env = "ZED_PKG_DRY_RUN")]
        dry_run: bool,
        /// Skip the clean-worktree check
        #[arg(long, env = "ZED_PKG_ALLOW_DIRTY")]
        allow_dirty: bool,
        /// Skip tag/commit verification (loud warning; for VCS systems
        /// zed cannot verify yet)
        #[arg(long, env = "ZED_PKG_SKIP_VCS_CHECKS")]
        skip_vcs_checks: bool,
    },
    /// Mark a published version as yanked: hidden from fresh resolution,
    /// still downloadable for existing lockfiles. --undo restores it.
    Yank {
        /// org/name@version
        spec: String,
        #[arg(long, env = "ZED_PKG_YANK_UNDO")]
        undo: bool,
    },
    /// Roundtrip-test this package the way a consumer would install it:
    /// pack it, publish it to a throwaway file:// registry, install it into a
    /// mock consumer project under your home dir, and run `publish.smoke_test`
    /// — optionally inside a fresh OCI container. Named after r2g
    /// (github.com/oresoftware/r2g); `zed test-local` is a compatibility alias.
    #[command(name = "r2g", alias = "test-local")]
    R2g {
        /// Run the install + smoke test inside a throwaway OCI container, so
        /// the artifact is exercised in a clean, host-independent environment
        /// (fresh $HOME, distro libraries, no host toolchain leaking in)
        #[arg(long, env = "ZED_PKG_R2G_DOCKER")]
        docker: bool,
        /// Base image for `--docker` (pick one with the runtime your smoke
        /// test needs, e.g. `node:22-slim`, `python:3.12-slim`, `rust:1-slim`)
        #[arg(long, env = "ZED_PKG_R2G_IMAGE", default_value = "debian:stable-slim")]
        image: String,
        /// Container runtime for `--docker`; auto-detected (docker, then
        /// podman) when unset
        #[arg(long, value_enum, env = "ZED_PKG_R2G_RUNTIME")]
        runtime: Option<ContainerRuntime>,
        /// Parent directory for the throwaway consumer project and its
        /// registry/store; defaults to `<zed home>/r2g` (i.e. ~/.zed-pkg/r2g)
        #[arg(long = "r2g-root", env = "ZED_PKG_R2G_ROOT")]
        root: Option<PathBuf>,
        /// Delete the throwaway workspace after a successful run instead of
        /// leaving it in your home dir for inspection (a failed run always
        /// leaves it behind)
        #[arg(long, env = "ZED_PKG_R2G_CLEAN")]
        clean: bool,
    },
    /// Replace this `zed` binary with the latest GitHub release for your
    /// platform (zed-docs issue #9)
    #[command(name = "self-update", alias = "update")]
    SelfUpdate {
        /// Only report whether an update is available; don't install
        #[arg(long, env = "ZED_PKG_UPDATE_CHECK")]
        check: bool,
        /// Reinstall even if already on the latest version
        #[arg(long, env = "ZED_PKG_UPDATE_FORCE")]
        force: bool,
        /// Skip the SHA256SUMS integrity check (unsafe; local testing only)
        #[arg(long, env = "ZED_PKG_UPDATE_SKIP_CHECKSUM")]
        skip_checksum: bool,
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
    use std::collections::BTreeSet;

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

    /// Walk every command and subcommand, asserting each flag has a
    /// `ZED_PKG_*` env fallback, and collecting the full set of envs.
    fn collect_flag_envs(cmd: &clap::Command, envs: &mut BTreeSet<String>) {
        for arg in cmd.get_arguments() {
            let Some(long) = arg.get_long() else { continue };
            if long == "help" || long == "version" {
                continue;
            }
            let env = arg
                .get_env()
                .unwrap_or_else(|| {
                    panic!(
                        "--{long} on `{}` lacks a ZED_PKG_* env fallback (flags-2-env)",
                        cmd.get_name()
                    )
                })
                .to_string_lossy()
                .to_string();
            assert!(
                env.starts_with("ZED_PKG_"),
                "--{long} env `{env}` must use the ZED_PKG_ prefix"
            );
            envs.insert(env);
        }
        for sub in cmd.get_subcommands() {
            collect_flag_envs(sub, envs);
        }
    }

    /// `.cli-flags.toml` is the declarative flags-2-env registry
    /// (github.com/oresoftware/flags-2-env). It must stay a byte-for-byte
    /// match with what clap actually exposes: every CLI flag is declared, and
    /// nothing declared is stale. This is what keeps `zed r2g`'s flags (and
    /// every other command's) documented and env-addressable.
    #[test]
    fn cli_flags_toml_is_in_sync_with_clap() {
        #[derive(serde::Deserialize)]
        struct FlagsFile {
            flag: Vec<FlagEntry>,
        }
        #[derive(serde::Deserialize)]
        struct FlagEntry {
            command: String,
            long: String,
            env: String,
        }

        let doc: FlagsFile = toml::from_str(include_str!("../.cli-flags.toml"))
            .expect(".cli-flags.toml must be valid TOML");

        let mut file_envs = BTreeSet::new();
        for entry in &doc.flag {
            assert!(!entry.long.is_empty(), "a flag entry is missing `long`");
            assert!(
                !entry.command.is_empty(),
                "flag --{} is missing `command`",
                entry.long
            );
            assert!(
                entry.env.starts_with("ZED_PKG_"),
                "flag --{} env `{}` must use the ZED_PKG_ prefix",
                entry.long,
                entry.env
            );
            assert!(
                file_envs.insert(entry.env.clone()),
                "duplicate env `{}` in .cli-flags.toml",
                entry.env
            );
        }

        let mut clap_envs = BTreeSet::new();
        collect_flag_envs(&Cli::command(), &mut clap_envs);

        let missing: Vec<&String> = clap_envs.difference(&file_envs).collect();
        let stale: Vec<&String> = file_envs.difference(&clap_envs).collect();
        assert!(
            missing.is_empty(),
            "flags in the CLI but not declared in .cli-flags.toml: {missing:?}"
        );
        assert!(
            stale.is_empty(),
            "flags declared in .cli-flags.toml but absent from the CLI: {stale:?}"
        );
    }
}
