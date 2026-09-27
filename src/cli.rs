use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

/// Every user-facing flag can also be set through a `ZED_PKG_*` environment
/// variable, following the flags-2-env convention
/// (github.com/flags-2-env/flags-2-env). Secret-bearing values stay env-only.
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

    /// zed home directory (store, cache, credentials); defaults to ~/.zpkg (legacy ~/.zed-pkg is retained when it is the only existing store)
    #[arg(long, global = true, env = "ZED_PKG_HOME")]
    pub home: Option<PathBuf>,

    /// Registry auth token from the environment only; overrides saved credentials.
    /// Secret values are deliberately not CLI options because argv is observable.
    #[arg(skip = std::env::var("ZED_PKG_TOKEN").ok())]
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
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_INTERACTIVE",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value = "false",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
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

    /// Fetch only from the configured registry; never fall back to a mirror.
    ///
    /// Use for a reproducibility audit, where "it installed" and "it installed
    /// from the canonical registry" are different claims and only the second
    /// one is being tested.
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_NO_MIRRORS",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
    pub no_mirrors: bool,

    /// Let a mirror answer metadata questions — resolving a range, reading a
    /// version — when the registry cannot, provided the answer carries a
    /// publisher signature that verifies.
    ///
    /// Off by default. Serving a *pinned* artifact from a mirror is safe
    /// without this, because the lockfile digest decides what is acceptable.
    /// Serving metadata is a genuine trust decision, so it is opt-in rather
    /// than something an operator discovers after the fact.
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_TRUST_MIRROR_METADATA",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
    pub trust_mirror_metadata: bool,

    /// Public HTTPS origin for guessable R2/CDN objects when the registry host
    /// is down. Overrides the default `https://cdn.zpkg.net`.
    #[arg(long, global = true, env = "ZED_PKG_R2_PUBLIC_BASE")]
    pub r2_public_base: Option<String>,

    /// Public R2 origin spelled as a hostname, full `https://…` URL, or
    /// Cloudflare `pub-<id>` account subdomain (`https://<id>.r2.dev`).
    #[arg(long, global = true, env = "ZED_PKG_R2_PUBLIC_KEY")]
    pub r2_public_key: Option<String>,

    /// Retry public R2 and GitHub when an HTTP registry is unreachable.
    /// Loopback and `file://` registries stay hermetic. Bare means true;
    /// `--source-fallback=false` disables it.
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_SOURCE_FALLBACK",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "true",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
    pub source_fallback: bool,
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

/// OCI runtime used by `zed r2g`.
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

/// Registry boundary exercised by `zed r2g`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum R2gRegistryMode {
    /// Publish only to a private file:// registry under the r2g workspace.
    Isolated,
    /// Publish to the configured HTTP(S) registry and install it back through
    /// the ordinary client path. This permanently claims that package version
    /// unless the server itself is an intentionally disposable instance.
    Server,
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
    /// Import or verify project-local manager state as an EnvironmentPlan.
    Mise,
    /// Import or verify project-local asdf configuration and Zed-owned provenance.
    Asdf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EnvironmentExportManagerArg {
    /// Export deterministic mise TOML from a schema-v2 plan.
    Mise,
    /// Export deterministic Devbox JSON and a Zed-owned receipt.
    Devbox,
    /// Export deterministic Flox manifest TOML and a Zed-owned receipt.
    Flox,
}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct WrapperCli {
    #[command(subcommand)]
    pub cmd: WrapperCmd,
}

#[derive(Debug, Subcommand)]
pub enum WrapperCmd {
    /// Execute the zed package manager CLI.
    Package {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Validate package manifest and lock metadata without network or filesystem mutation
    Validate {
        /// Package manifest to validate
        #[arg(long, env = "ZED_PKG_VALIDATE_MANIFEST", default_value = ".zpkg.toml")]
        manifest: PathBuf,
        /// Package lockfile to validate when present
        #[arg(long, env = "ZED_PKG_VALIDATE_LOCK", default_value = ".zpkg.lock")]
        lock: PathBuf,
        /// Fail when the lockfile is absent
        #[arg(long, env = "ZED_PKG_VALIDATE_REQUIRE_LOCK")]
        require_lock: bool,
        /// Emit deterministic machine-readable JSON
        #[arg(long, env = "ZED_PKG_VALIDATE_JSON")]
        json: bool,
    },
    /// Create a project directory and .zpkg.toml manifest (current directory by default)
    Init {
        /// Project directory to create or initialize. Relative paths are
        /// resolved below the current working directory.
        #[arg(value_name = "PROJECT", env = "ZED_PKG_INIT_PROJECT")]
        project: Option<PathBuf>,
        #[arg(long, env = "ZED_PKG_ORG")]
        org: Option<String>,
        #[arg(long, env = "ZED_PKG_NAME")]
        name: Option<String>,
    },
    /// Add a dependency (org/name[@semver-req]) and install it
    Add { spec: String },
    /// Inspect and exercise the fallback sources for this project's packages
    Mirror {
        #[command(subcommand)]
        cmd: MirrorCmd,
    },
    /// Manage the publisher signing keys that let mirrors serve metadata
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
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
        /// Install a project-owned CLI runtime. Repeat for multiple tools;
        /// built-in aliases currently include nodejs and python3.
        #[arg(long, value_name = "TOOL", env = "ZED_PKG_CLI", action = clap::ArgAction::Append)]
        cli: Vec<String>,
        /// Exact CLI runtime target used for cross-platform image builds.
        #[arg(long, env = "ZED_PKG_CLI_TARGET")]
        cli_target: Option<String>,
        /// CLI runtimes default to a self-contained project copy so they can
        /// cross OCI stages without Zed's global store.
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_CLI_INSTALL_MODE",
            default_value = "copy"
        )]
        cli_install_mode: InstallMode,
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
        #[arg(
            long,
            env = "ZED_PKG_ALLOW_BUILD",
            num_args = 0..=1,
            default_missing_value = "true",
            default_value = "false",
            value_parser = clap::builder::BoolishValueParser::new(),
            action = clap::ArgAction::Set
        )]
        allow_build: bool,
        /// Install host-native prerequisites declared by packages. This may
        /// invoke an OS package manager and is independent from build-hook
        /// consent.
        #[arg(long, env = "ZED_PKG_ALLOW_NATIVE_DEPS")]
        allow_native_deps: bool,
        /// Run package-authored pre-install and post-install commands in a
        /// writable staging copy. Off by default because hooks are arbitrary
        /// author code.
        #[arg(long, env = "ZED_PKG_ALLOW_INSTALL_HOOKS")]
        allow_install_hooks: bool,
        /// Pin the native package manager selected for the complete dependency
        /// graph (for example apt, apk, brew, or nix). Omitted = detect one
        /// manager supported by every package that declares native deps.
        #[arg(long, env = "ZED_PKG_NATIVE_MANAGER")]
        native_manager: Option<String>,
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
            env = "ZED_PKG_ALLOW_NO_MANIFEST",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true",
            default_value = "false",
            value_parser = clap::builder::BoolishValueParser::new(),
            action = clap::ArgAction::Set
        )]
        allow_no_manifest: bool,
        /// Install single-language packages whose ecosystem this project does
        /// not have (e.g. a -java client into a Node project). Off by default;
        /// pass explicitly when cross-ecosystem placement is intentional.
        #[arg(long, env = "ZED_PKG_ALLOW_ECOSYSTEM_MISMATCH")]
        allow_ecosystem_mismatch: bool,
    },
    /// Remove materialized packages and language adapter state while preserving lock metadata
    Uninstall,
    /// Export a lockfile for another supported package ecosystem
    Export {
        #[command(subcommand)]
        cmd: ExportCmd,
    },
    /// Import external ecosystem package manifests/locks into Zed authority
    Import {
        #[command(subcommand)]
        cmd: ImportCmd,
    },
    /// Import a language/version/environment manager into Zed authority
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// Overtake checked-in Git submodules into Zed package authority
    Overtake {
        /// Remove eligible gitlinks from the index after successfully recording
        /// them in `.zpkg.toml` and `.zpkg.lock`. Without this flag, `overtake`
        /// is a deterministic dry conversion/verification pass.
        #[arg(long, env = "ZED_PKG_OVERTAKE_WRITE")]
        write: bool,
    },
    /// Print shell completion script
    Completions { shell: CompletionShell },
    /// Run manifest-defined project tasks
    Task {
        /// Optional schema-v2 task plan. When omitted, `[tasks.*]` from `.zpkg.toml` is used.
        #[arg(long, env = "ZED_TASK_PLAN")]
        plan: Option<PathBuf>,
        /// Emit machine-readable output where supported.
        #[arg(long, env = "ZED_TASK_JSON")]
        json: bool,
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Run this package's [build] command once (cached; --force to rebuild)
    Build {
        #[arg(long)]
        force: bool,
        /// Install host-native prerequisites declared by this package before the build command.
        #[arg(long, env = "ZED_PKG_ALLOW_NATIVE_DEPS")]
        allow_native_deps: bool,
        /// Run package-authored pre-install and post-install commands for any dependencies materialized before the build.
        #[arg(long, env = "ZED_PKG_ALLOW_INSTALL_HOOKS")]
        allow_install_hooks: bool,
        /// Pin the native package manager used for dependency native prerequisites.
        #[arg(long, env = "ZED_PKG_NATIVE_MANAGER")]
        native_manager: Option<String>,
    },
    /// Run a command from [scripts] or an installed binary
    Run {
        command: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Garbage-collect unreferenced artifact versions from the global store
    Gc {
        #[arg(long, value_name = "DURATION", default_value = "30d")]
        older_than: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Find/search packages
    Find {
        query: String,
    },
    /// Print the resolved dependency tree
    Tree {
        package: Option<String>,
        #[arg(long, default_value_t = 4)]
        depth: usize,
        #[arg(long)]
        json: bool,
    },
    /// Explain why a package is installed
    Why {
        package: String,
        #[arg(long)]
        json: bool,
    },
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
    /// pack it, publish it to a private file:// registry by default (or the
    /// explicitly configured HTTP(S) registry in server mode), install it into
    /// a mock consumer project, and run `publish.smoke_test` — optionally
    /// inside a fresh OCI container. Named after r2g
    /// (github.com/oresoftware/r2g); `zed test-local` is a compatibility alias.
    #[command(name = "r2g", alias = "test-local")]
    R2g {
        /// Registry boundary to exercise. `isolated` is the safe default.
        /// `server` publishes permanently to the configured HTTP(S) registry.
        #[arg(
            long,
            value_enum,
            env = "ZED_PKG_R2G_REGISTRY_MODE",
            default_value = "isolated"
        )]
        registry_mode: R2gRegistryMode,
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
        /// leaving it in your home dir for inspection. In server mode this
        /// does not delete or yank the version persisted by the registry.
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
    /// Sign up (same as `zed auth signup` / `zed auth register`)
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
        #[arg(long, env = "ZED_TASK_JSON")]
        json: bool,
    },
    /// Export a schema-v2 EnvironmentPlan to deterministic manager configuration.
    Export {
        #[arg(value_enum)]
        manager: EnvironmentExportManagerArg,
        /// Project-local schema-v2 EnvironmentPlan. Devbox/Flox default to `.zed/environment-plan.json`; mise requires this flag.
        #[arg(long, env = "ZED_PKG_ENV_PLAN")]
        plan: Option<PathBuf>,
        /// Project-local manager output path. Defaults are manager-specific.
        #[arg(long, env = "ZED_PKG_ENV_OUTPUT")]
        output: Option<PathBuf>,
        /// Zed-owned deterministic receipt path for Devbox/Flox export.
        #[arg(long, env = "ZED_PKG_ENV_RECEIPT")]
        receipt: Option<PathBuf>,
        /// Verify that the mise output already equals the deterministic projection.
        #[arg(long, env = "ZED_PKG_ENV_CHECK")]
        check: bool,
        /// Transactionally create/update a Zed-owned mise view.
        #[arg(long, env = "ZED_PKG_ENV_WRITE")]
        write: bool,
        /// Emit a machine-readable export result.
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
    /// Verify project-local package manager ecosystem integration.
    Integrate {
        /// Fail instead of writing missing integration files.
        #[arg(long, env = "ZED_PKG_INTEGRATE_CHECK")]
        check: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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
        /// Write a new self-contained HTML report without overwriting an existing file
        #[arg(long, env = "ZED_PKG_RELEASE_HTML", conflicts_with = "json")]
        html: Option<PathBuf>,
        /// Release track to resolve every native route against
        #[arg(long, value_enum, env = "ZED_PKG_RELEASE_CHANNEL")]
        channel: Option<ChannelArg>,
        /// Candidate number within a pre-release channel (rc.1, rc.2, ...)
        #[arg(long, default_value_t = 1, env = "ZED_PKG_RELEASE_ITERATION")]
        iteration: u32,
    },
    /// Run fixed, credential-free native package preflight adapters
    Preflight {
        /// Restrict to one target from `[targets.*]`
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
    },
    /// Upload every native route to its ecosystem registry over that
    /// registry's own HTTP API
    Publish {
        #[arg(long, value_enum, env = "ZED_PKG_RELEASE_CHANNEL")]
        channel: Option<ChannelArg>,
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
    /// Sign in and save a refreshable local session when immediately confirmed
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
pub enum MirrorCmd {
    /// Show the mirrors that would be tried, in order, for this project
    List {
        /// Emit deterministic machine-readable JSON
        #[arg(
            long,
            env = "ZED_PKG_MIRROR_JSON",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true",
            default_value = "false",
            value_parser = clap::builder::BoolishValueParser::new(),
            action = clap::ArgAction::Set
        )]
        json: bool,
    },
    /// Probe every mirror for every locked package and report what answers
    ///
    /// Run this while things are healthy. A fallback nobody has ever
    /// exercised is a fallback that does not work, and the moment you find
    /// out is the moment you needed it.
    Check {
        /// Check only this package (`org/name`)
        #[arg(long, value_name = "PACKAGE", env = "ZED_PKG_MIRROR_PACKAGE")]
        package: Option<String>,
        #[arg(
            long,
            env = "ZED_PKG_MIRROR_JSON",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true",
            default_value = "false",
            value_parser = clap::builder::BoolishValueParser::new(),
            action = clap::ArgAction::Set
        )]
        json: bool,
    },
    /// Recover the mirror set from any reachable host, without the registry
    Bootstrap {
        /// Base URL to ask; defaults to every known mirror in turn
        #[arg(long, value_name = "URL", env = "ZED_PKG_MIRROR_BOOTSTRAP_URL")]
        url: Option<String>,
    },
    /// Build a `file://` mirror of everything this project pins
    ///
    /// The output is a complete offline source: point `--registry` or a
    /// `directory` mirror at it and the project installs with no network.
    Sync {
        /// Directory to write. Created if absent; existing artifacts are kept.
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeyCmd {
    /// Create a publisher signing key and print the public half to enroll
    Generate {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: String,
        /// Short stable label for this key, e.g. `acme-2026`
        #[arg(long, value_name = "ID", env = "ZED_PKG_KEY_ID")]
        key_id: String,
    },
    /// List the signing keys this machine holds for an org
    List {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: String,
    },
    /// Print the public half of one key, ready to paste into `.zpkg.toml`
    Show {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: String,
        #[arg(long, value_name = "ID", env = "ZED_PKG_KEY_ID")]
        key_id: String,
    },
    /// Upload the org's public key set to the registry
    Enroll {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: String,
        #[arg(long, value_name = "ID", env = "ZED_PKG_KEY_ID")]
        key_id: String,
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
    /// A secret *value* must never be a CLI option, because argv is readable
    /// by any process on the host. This pins the shape the failure message in
    /// `ops::login` describes: the two drifted apart once, and the message
    /// told operators to pass a `--token` that has never existed.
    ///
    /// Every spelling an argument answers to is checked — its id, its long
    /// name and each alias — since an alias is as observable as the name. The
    /// list is exact rather than a substring match on purpose: `--password-stdin`
    /// is the safe alternative this rule exists to push people toward, and a
    /// substring rule would forbid it.
    #[test]
    fn secret_values_are_not_command_line_options() {
        const FORBIDDEN: [&str; 8] = [
            "token",
            "password",
            "secret",
            "api-token",
            "auth-token",
            "access-token",
            "api-key",
            "private-key",
        ];
        let mut pending = vec![Cli::command()];
        while let Some(current) = pending.pop() {
            for argument in current.get_arguments() {
                let spellings = std::iter::once(argument.get_id().as_str().replace('_', "-"))
                    .chain(argument.get_long().map(str::to_owned))
                    .chain(
                        argument
                            .get_all_aliases()
                            .unwrap_or_default()
                            .into_iter()
                            .map(str::to_owned),
                    );
                for spelling in spellings {
                    assert!(
                        !FORBIDDEN.contains(&spelling.as_str()),
                        "`{spelling}` on `{}` would put a secret value in argv",
                        current.get_name()
                    );
                }
            }
            pending.extend(current.get_subcommands().cloned());
        }
    }

    /// The guidance an operator sees has to name a mechanism that exists.
    #[test]
    fn auth_guidance_names_real_commands() {
        let root = Cli::command();
        let subcommands = root
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<Vec<_>>();
        assert!(subcommands.contains(&"auth"));
        assert!(subcommands.contains(&"login"));
        assert!(subcommands.contains(&"logout"));
        let auth = root
            .get_subcommands()
            .find(|command| command.get_name() == "auth")
            .expect("auth subcommand");
        let auth_subcommands = auth
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<Vec<_>>();
        assert!(auth_subcommands.contains(&"login"));
        assert!(auth_subcommands.contains(&"signout"));
    }

    #[test]
    fn cli_parses_release_preflight_target() {
        let cli = Cli::try_parse_from(["zed", "release", "preflight", "--target", "rust"])
            .expect("release preflight target parses");
        assert!(matches!(
            cli.cmd,
            Cmd::Release {
                cmd: ReleaseCmd::Preflight {
                    target: Some(ref target),
                },
            } if target == "rust"
        ));
    }
}
