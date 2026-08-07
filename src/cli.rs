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

    /// shared-auth base URL; defaults to <registry>/shared-auth
    #[arg(long, global = true, env = "ZED_PKG_AUTH_URL")]
    pub auth_url: Option<String>,

    /// Supabase project URL used for provider login/signup
    #[arg(long, global = true, env = "ZED_PKG_SUPABASE_URL")]
    pub supabase_url: Option<String>,

    /// Supabase publishable/anon key (never a service-role key)
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_SUPABASE_KEY",
        hide_env_values = true
    )]
    pub supabase_key: Option<String>,

    /// Confirm every mutating lifecycle step in a real terminal. A declined
    /// prompt, EOF, or redirected stdin fails closed before that step.
    #[arg(long, global = true, env = "ZED_PKG_INTERACTIVE")]
    pub interactive: bool,

    /// Enable Git submodule compatibility for commands that consume Git
    /// transport metadata. `install` synchronizes recursively before package
    /// resolution; `overtake` imports eligible submodules into Zed authority.
    /// Bare means true; use `--git-submodules=false` to override an enabled
    /// environment value.
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_GIT_SUBMODULES",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
    pub git_submodules: bool,
}

/// Contextual adapters translate zed's universal layout into what a
/// language's toolchain expects, per the "structural translation" goal:
/// the same artifact lands where Node, the JVM, or plain zed_modules/
/// consumers respectively look for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
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
    /// Additionally write .zed/go.work so the Go toolchain sees installed
    /// modules; use with GOWORK="$(pwd)/.zed/go.work"
    Go,
    /// Additionally write .zed/pythonpath; use with
    /// PYTHONPATH="$(cat .zed/pythonpath)"
    Python,
    /// Additionally write .zed/cargo-paths.toml, a `paths = [...]` fragment to
    /// include from .cargo/config.toml (Cargo has no env-var path override)
    Rust,
    /// Additionally write .zed/pub-deps.yaml, path dependencies to merge into
    /// pubspec.yaml (pub has no env-var path override)
    Dart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InstallMode {
    /// Symlink from the global store into zed_modules/ (pnpm-style)
    Symlink,
    /// Copy files out of the store; use inside container image builds so
    /// layers stay self-contained across multi-stage COPYs
    Copy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthProvider {
    /// Use Supabase when its project URL and publishable key are configured,
    /// otherwise use shared-auth directly
    Auto,
    /// Authenticate directly against shared-auth's local account authority
    SharedAuth,
    /// Authenticate with Supabase Auth, then exchange into shared-auth while
    /// retaining the Supabase session as the independent fallback authority
    Supabase,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => clap_complete::Shell::Bash,
            CompletionShell::Zsh => clap_complete::Shell::Zsh,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EnvironmentManagerArg {
    /// Import or verify project-local mise configuration.
    Mise,
    /// Import or verify project-local asdf configuration and Zed-owned provenance.
    Asdf,
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
    /// Resolve and install dependencies into the selected project
    #[command(alias = "i")]
    Install {
        /// Package specs (`org/name[@requirement]`). When no manifest exists,
        /// these become direct dependencies in a generated consumer manifest
        /// by default. A human-authored manifest is never edited here; use
        /// `zed add` to persist dependencies in an authored project.
        #[arg(value_name = "PACKAGE")]
        specs: Vec<String>,
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
        /// Do not create a new .zpkg.toml when installing into a project that
        /// does not have one. The lockfile, integrity checks, materialization,
        /// adapters, frozen policy, and explicitly allowed builds still run.
        #[arg(
            long = "do-not-write-new-manifest",
            visible_aliases = ["allow-no-manifest", "skip-manifest"],
            env = "ZED_PKG_ALLOW_NO_MANIFEST"
        )]
        allow_no_manifest: bool,
        /// Install single-language packages whose ecosystem this project does
        /// not have (e.g. a -java client into a Node project). Off by default:
        /// the wrong-language package is invisible to the toolchain, so the
        /// mismatch is almost always a mistake worth failing on
        #[arg(long, env = "ZED_PKG_ALLOW_ECOSYSTEM_MISMATCH")]
        allow_ecosystem_mismatch: bool,
    },
    /// Remove installed dependency trees while retaining .zpkg.toml and
    /// .zpkg.lock so `zed install --frozen` can restore them exactly.
    #[command(alias = "un")]
    Uninstall {
        /// Packages to unmaterialize (`org/name`). Omit to uninstall all
        /// packages currently pinned by the lockfile.
        #[arg(value_name = "PACKAGE")]
        specs: Vec<String>,
    },
    /// Import or verify project-local developer-environment configuration.
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// List, inspect, graph, or execute native schema-v2 project tasks.
    Task {
        /// Project-local schema-v2 environment plan; conventional names are discovered when omitted.
        #[arg(long, env = "ZED_TASK_PLAN")]
        plan: Option<PathBuf>,
        /// Emit stable machine-readable JSON. Live command execution requires human streaming output.
        #[arg(long, env = "ZED_TASK_JSON")]
        json: bool,
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Generate a completion script from the same typed command model used at runtime
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
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
    /// Plan a coordinated Zed + native-registry release without credentials or uploads
    Release {
        #[command(subcommand)]
        cmd: ReleaseCmd,
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
    /// Sign in (same as `zed auth login`)
    #[command(alias = "signin")]
    Login {
        #[arg(long, env = "ZED_PKG_AUTH_EMAIL")]
        email: Option<String>,
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_AUTH_PROVIDER",
            default_value = "auto"
        )]
        provider: AuthProvider,
        #[arg(long, env = "ZED_PKG_AUTH_PASSWORD_STDIN")]
        password_stdin: bool,
    },
    /// Sign up (same as `zed auth signup`)
    #[command(alias = "register")]
    Signup {
        #[arg(long, env = "ZED_PKG_AUTH_EMAIL")]
        email: Option<String>,
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_AUTH_PROVIDER",
            default_value = "auto"
        )]
        provider: AuthProvider,
        #[arg(long, env = "ZED_PKG_AUTH_DISPLAY_NAME")]
        display_name: Option<String>,
        #[arg(long, env = "ZED_PKG_AUTH_PASSWORD_STDIN")]
        password_stdin: bool,
    },
    /// Sign out (same as `zed auth logout` / `zed auth signout`)
    #[command(alias = "signout")]
    Logout,
    /// Human account authentication through shared-auth and Supabase
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
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
pub enum EnvCmd {
    /// Import the supported project-local manager state as an EnvironmentPlan.
    Import {
        #[arg(value_enum)]
        manager: EnvironmentManagerArg,
        /// Project-local manager config; auto-detected only when unambiguous.
        #[arg(long, env = "ZED_PKG_ENV_CONFIG")]
        config: Option<PathBuf>,
        /// Project-local manager lockfile; otherwise derived from the config name.
        #[arg(long, env = "ZED_PKG_ENV_LOCK")]
        lock: Option<PathBuf>,
        /// Require complete locked identities and portable frozen validation.
        #[arg(long, env = "ZED_PKG_FROZEN")]
        frozen: bool,
        /// Emit the normalized EnvironmentPlan as JSON.
        #[arg(long, env = "ZED_PKG_ENV_JSON")]
        json: bool,
    },
    /// Verify manager config/lock coverage and the normalized plan digest.
    Verify {
        #[arg(value_enum)]
        manager: EnvironmentManagerArg,
        /// Project-local manager config; auto-detected only when unambiguous.
        #[arg(long, env = "ZED_PKG_ENV_CONFIG")]
        config: Option<PathBuf>,
        /// Project-local manager lockfile; otherwise derived from the config name.
        #[arg(long, env = "ZED_PKG_ENV_LOCK")]
        lock: Option<PathBuf>,
        /// Require complete locked identities and portable frozen validation.
        #[arg(long, env = "ZED_PKG_FROZEN")]
        frozen: bool,
        /// Emit a machine-readable verification result.
        #[arg(long, env = "ZED_PKG_ENV_JSON")]
        json: bool,
    },
}

/// Release track. How this becomes a version string is the destination
/// registry's business — npm wants `1.4.0-rc.1` plus a dist-tag, PyPI wants
/// `1.4.0rc1`, Maven wants `1.4.0-RC1` — so the channel is named here and
/// resolved per host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ChannelArg {
    Stable,
    Rc,
    Beta,
    Alpha,
    Nightly,
    Snapshot,
}

impl From<ChannelArg> for zed_interfaces::native_host::ReleaseChannel {
    fn from(value: ChannelArg) -> Self {
        use zed_interfaces::native_host::ReleaseChannel as C;
        match value {
            ChannelArg::Stable => C::Stable,
            ChannelArg::Rc => C::Rc,
            ChannelArg::Beta => C::Beta,
            ChannelArg::Alpha => C::Alpha,
            ChannelArg::Nightly => C::Nightly,
            ChannelArg::Snapshot => C::Snapshot,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum TaskCmd {
    /// List project tasks in deterministic name order.
    List {
        /// Include tasks marked hidden.
        #[arg(long, env = "ZED_TASK_ALL")]
        all: bool,
    },
    /// Show one task's aliases, dependencies, cache policy, and description.
    Info { task: String },
    /// Print the validated task dependency and invocation graph.
    Graph { task: String },
    /// Execute one task and its validated dependency graph.
    Run {
        task: String,
        /// Plan commands and cache decisions without subprocesses or mutation.
        #[arg(long, env = "ZED_TASK_DRY_RUN")]
        dry_run: bool,
        /// Approve an explicit task confirmation requirement.
        #[arg(long, env = "ZED_TASK_YES")]
        yes: bool,
        /// Maximum number of concurrently running task commands.
        #[arg(
            long,
            env = "ZED_TASK_JOBS",
            default_value_t = 1,
            value_parser = crate::task_cli::parse_positive_jobs
        )]
        jobs: usize,
        /// Disable content-verified incremental cache reads and writes.
        #[arg(long, env = "ZED_TASK_NO_CACHE")]
        no_cache: bool,
        /// Arguments are exposed through ZED_TASK_ARGC, ZED_TASK_ARGS_JSON, and ZED_TASK_ARG_<n>.
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReleaseCmd {
    /// Print the deterministic release set derived from `.zpkg.toml`
    Plan {
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_RELEASE_JSON")]
        json: bool,
        /// Release track to resolve every native route against
        #[arg(
            long,
            value_enum,
            default_value = "stable",
            env = "ZED_PKG_RELEASE_CHANNEL"
        )]
        channel: ChannelArg,
        /// Candidate number within a pre-release channel (rc.1, rc.2, ...)
        #[arg(long, default_value_t = 1, env = "ZED_PKG_RELEASE_ITERATION")]
        iteration: u32,
    },
    /// Run fixed, credential-free native package preflight adapters
    Preflight,
    /// Upload every native route to its ecosystem registry over that
    /// registry's own HTTP API
    Publish {
        #[arg(
            long,
            value_enum,
            default_value = "stable",
            env = "ZED_PKG_RELEASE_CHANNEL"
        )]
        channel: ChannelArg,
        #[arg(long, default_value_t = 1, env = "ZED_PKG_RELEASE_ITERATION")]
        iteration: u32,
        /// Print the exact requests, with credentials redacted, and send none
        #[arg(long, env = "ZED_PKG_DRY_RUN")]
        dry_run: bool,
        /// Restrict to one target from `[targets.*]`
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
    },
    /// List the versions each native route's registry already serves
    Versions {
        /// Restrict to one target from `[targets.*]`
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCmd {
    /// Sign in and save a refreshable local session
    #[command(alias = "signin")]
    Login {
        #[arg(long, env = "ZED_PKG_AUTH_EMAIL")]
        email: Option<String>,
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_AUTH_PROVIDER",
            default_value = "auto"
        )]
        provider: AuthProvider,
        /// Read the password as one line from stdin instead of prompting
        #[arg(long, env = "ZED_PKG_AUTH_PASSWORD_STDIN")]
        password_stdin: bool,
    },
    /// Create an account and save its session when immediately confirmed
    #[command(alias = "register")]
    Signup {
        #[arg(long, env = "ZED_PKG_AUTH_EMAIL")]
        email: Option<String>,
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_AUTH_PROVIDER",
            default_value = "auto"
        )]
        provider: AuthProvider,
        #[arg(long, env = "ZED_PKG_AUTH_DISPLAY_NAME")]
        display_name: Option<String>,
        /// Read the password as one line from stdin instead of prompting
        #[arg(long, env = "ZED_PKG_AUTH_PASSWORD_STDIN")]
        password_stdin: bool,
    },
    /// Revoke remote sessions when possible and always delete local tokens
    #[command(alias = "logout")]
    Signout,
    /// Save a legacy opaque registry token
    ImportToken,
    /// Show the locally authenticated identity and token expiry
    Status,
    /// Rotate refresh tokens now
    Refresh,
    /// Print the current access token, refreshing it first when needed
    Token,
}

#[derive(Debug, Subcommand)]
pub enum OrgCmd {
    /// Claim an org namespace on the registry
    Claim { slug: String },
    /// Show the org's audit trail — who published, yanked, or claimed, and
    /// when. Requires an `owner` (or admin) token (zed-docs issue #7)
    Audit {
        slug: String,
        /// Maximum entries to show, newest first (server clamps to 1000)
        #[arg(long, env = "ZED_PKG_AUDIT_LIMIT")]
        limit: Option<u64>,
    },
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

    use clap::{CommandFactory, Parser};

    use super::{AuthCmd, Cli, Cmd};

    #[test]
    fn flat_and_nested_auth_spellings_dispatch_identically() {
        fn action(words: &[&str]) -> &'static str {
            let cli = Cli::try_parse_from(
                std::iter::once("zed")
                    .chain(words.iter().copied())
                    .chain(["--email", "person@example.com"]),
            )
            .unwrap();
            match cli.cmd {
                Cmd::Login { .. }
                | Cmd::Auth {
                    cmd: AuthCmd::Login { .. },
                } => "login",
                Cmd::Signup { .. }
                | Cmd::Auth {
                    cmd: AuthCmd::Signup { .. },
                } => "signup",
                Cmd::Logout
                | Cmd::Auth {
                    cmd: AuthCmd::Signout,
                } => "logout",
                other => panic!("unexpected auth command: {other:?}"),
            }
        }

        for words in [
            &["login"][..],
            &["signin"],
            &["auth", "login"],
            &["auth", "signin"],
        ] {
            assert_eq!(action(words), "login", "{words:?}");
        }
        for words in [
            &["signup"][..],
            &["register"],
            &["auth", "signup"],
            &["auth", "register"],
        ] {
            assert_eq!(action(words), "signup", "{words:?}");
        }

        fn logout_action(words: &[&str]) -> &'static str {
            let cli =
                Cli::try_parse_from(std::iter::once("zed").chain(words.iter().copied())).unwrap();
            match cli.cmd {
                Cmd::Logout
                | Cmd::Auth {
                    cmd: AuthCmd::Signout,
                } => "logout",
                other => panic!("unexpected logout command: {other:?}"),
            }
        }
        for words in [
            &["logout"][..],
            &["signout"],
            &["auth", "logout"],
            &["auth", "signout"],
        ] {
            assert_eq!(logout_action(words), "logout", "{words:?}");
        }
    }

    #[test]
    fn install_accepts_specs_and_canonical_and_legacy_manifest_spellings() {
        for bypass in [
            "--do-not-write-new-manifest",
            "--allow-no-manifest",
            "--skip-manifest",
        ] {
            let cli = Cli::try_parse_from(["zed", "install", "acme/http-kit@^1", bypass]).unwrap();
            match cli.cmd {
                Cmd::Install {
                    specs,
                    allow_no_manifest,
                    ..
                } => {
                    assert_eq!(specs, ["acme/http-kit@^1"]);
                    assert!(allow_no_manifest);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn git_submodule_switch_is_global_boolish_and_does_not_consume_specs() {
        for args in [
            ["zed", "--git-submodules", "install", "acme/http-kit@^1"],
            ["zed", "install", "--git-submodules", "acme/http-kit@^1"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.globals.git_submodules, "{args:?}");
            match cli.cmd {
                Cmd::Install { specs, .. } => {
                    assert_eq!(specs, ["acme/http-kit@^1"]);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }

        let cli = Cli::try_parse_from([
            "zed",
            "install",
            "--git-submodules=false",
            "acme/http-kit@^1",
        ])
        .unwrap();
        assert!(!cli.globals.git_submodules);
        assert!(matches!(cli.cmd, Cmd::Install { .. }));
    }

    #[test]
    fn environment_import_and_verify_are_typed() {
        for manager in ["mise", "asdf"] {
            for action in ["import", "verify"] {
                let cli = Cli::try_parse_from([
                    "zed",
                    "env",
                    action,
                    manager,
                    "--config",
                    if manager == "mise" {
                        "mise.toml"
                    } else {
                        ".tool-versions"
                    },
                    "--lock",
                    if manager == "mise" {
                        "mise.lock"
                    } else {
                        ".zed/asdf.lock.toml"
                    },
                    "--frozen",
                    "--json",
                ])
                .unwrap();
                assert!(matches!(cli.cmd, Cmd::Env { .. }));
            }
        }
    }

    #[test]
    fn task_commands_are_typed_and_reject_zero_concurrency() {
        for args in [
            vec!["zed", "task", "list", "--all"],
            vec!["zed", "task", "--json", "info", "build"],
            vec!["zed", "task", "graph", "build"],
            vec!["zed", "task", "run", "build", "--dry-run", "--jobs", "2"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(cli.cmd, Cmd::Task { .. }));
        }

        let error =
            Cli::try_parse_from(["zed", "task", "run", "build", "--jobs", "0"]).unwrap_err();
        assert!(error.to_string().contains("at least one"));
    }

    #[test]
    fn completion_shells_are_typed_positionals() {
        for shell in ["bash", "zsh"] {
            let cli = Cli::try_parse_from(["zed", "completions", shell]).unwrap();
            assert!(matches!(cli.cmd, Cmd::Completions { .. }));
        }
    }

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
                env.starts_with("ZED_PKG_") || env.starts_with("ZED_TASK_"),
                "--{long} env `{env}` must use a registered ZED_PKG_ or ZED_TASK_ namespace"
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
        assert_eq!(
            env_of("git-submodules").as_deref(),
            Some("ZED_PKG_GIT_SUBMODULES")
        );
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
                env.starts_with("ZED_PKG_") || env.starts_with("ZED_TASK_"),
                "--{long} env `{env}` must use a registered ZED_PKG_ or ZED_TASK_ namespace"
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
        let doc: toml::Value = toml::from_str(include_str!("../.cli-flags.toml"))
            .expect(".cli-flags.toml must be valid TOML");
        let git_submodules = doc
            .as_table()
            .and_then(|root| root.get("flags"))
            .and_then(toml::Value::as_table)
            .and_then(|flags| flags.get("git_submodules"))
            .and_then(toml::Value::as_table)
            .expect("git_submodules must remain a root/global flags2env entry");
        assert_eq!(
            git_submodules.get("env").and_then(toml::Value::as_str),
            Some("ZED_PKG_GIT_SUBMODULES")
        );
        assert_eq!(
            git_submodules.get("type").and_then(toml::Value::as_str),
            Some("bool")
        );

        fn collect_file_envs(value: &toml::Value, envs: &mut BTreeSet<String>) {
            let Some(table) = value.as_table() else {
                return;
            };
            if let Some(flags) = table.get("flags").and_then(toml::Value::as_table) {
                for (name, flag) in flags {
                    let env = flag
                        .get("env")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_else(|| panic!("flag `{name}` is missing `env`"));
                    assert!(
                        env.starts_with("ZED_PKG_") || env.starts_with("ZED_TASK_"),
                        "flag --{} env `{env}` must use a registered ZED_PKG_ or ZED_TASK_ namespace",
                        name.replace('_', "-")
                    );
                    assert!(
                        envs.insert(env.to_string()),
                        "duplicate env `{env}` in .cli-flags.toml"
                    );
                }
            }
            for child in table.values() {
                collect_file_envs(child, envs);
            }
        }

        let mut file_envs = BTreeSet::new();
        collect_file_envs(&doc, &mut file_envs);

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

    /// Every command path the CLI exposes, as space-joined words
    /// (`["install", "org claim", "org audit", ...]`). Aliases are skipped —
    /// the canonical name is what must be documented.
    fn command_paths(cmd: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            if name == "help" {
                continue;
            }
            let path = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix} {name}")
            };
            if sub.get_subcommands().next().is_some() {
                // A group (`org`, `store`): document its leaves, not the group.
                command_paths(sub, &path, out);
            } else {
                out.push(path);
            }
        }
    }

    /// The README's command table is the front door: every shipped command
    /// must appear in it. This repo is developed by several people/sessions
    /// at once, so a command can land with its docs silently missing (that is
    /// exactly how `zed org audit` shipped undocumented). Same idea as the
    /// `.cli-flags.toml` gate above, applied to commands.
    #[test]
    fn readme_documents_every_command() {
        let readme = include_str!("../README.md");
        let mut paths = Vec::new();
        command_paths(&Cli::command(), "", &mut paths);
        assert!(
            paths.len() > 10,
            "command discovery looks broken: {paths:?}"
        );

        let mut undocumented = Vec::new();
        for path in &paths {
            let mut words = path.rsplitn(2, ' ');
            let leaf = words.next().unwrap_or(path);
            let parent = words.next();
            // A leaf is documented either by its own row (`zed org audit`) or
            // by a grouped row that lists it (`zed store status|path|prune`).
            let documented = readme.lines().any(|line| {
                let anchor = match parent {
                    Some(parent) => format!("zed {parent} "),
                    None => "zed ".to_string(),
                };
                line.contains("| `zed ") && line.contains(&anchor) && mentions_word(line, leaf)
            });
            if !documented {
                undocumented.push(path.clone());
            }
        }
        assert!(
            undocumented.is_empty(),
            "commands missing from the README table: {undocumented:?}"
        );
    }

    /// Whole-word match so `zed run` is not satisfied by `zed r2g`, and
    /// `path` in `status\\|path\\|prune` still counts.
    fn mentions_word(haystack: &str, word: &str) -> bool {
        haystack.match_indices(word).any(|(idx, _)| {
            let before = haystack[..idx].chars().next_back();
            let after = haystack[idx + word.len()..].chars().next();
            let boundary = |c: Option<char>| match c {
                None => true,
                Some(c) => !(c.is_ascii_alphanumeric() || c == '-'),
            };
            boundary(before) && boundary(after)
        })
    }
}
