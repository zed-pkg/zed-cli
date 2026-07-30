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
        /// Transient package specs (`org/name[@requirement]`). With no manifest
        /// these form the in-memory consumer plan; an existing manifest is
        /// never edited by this command (`zed add` persists dependencies).
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
        /// Proceed without prompting when no .zpkg.toml can be found.
        #[arg(
            long,
            visible_alias = "skip-manifest",
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
pub enum ReleaseCmd {
    /// Print the deterministic release set derived from `.zpkg.toml`
    Plan {
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_RELEASE_JSON")]
        json: bool,
    },
    /// Run fixed, credential-free npm and crates.io package preflight adapters
    Preflight,
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
    fn install_accepts_specs_and_both_manifestless_bypass_spellings() {
        for bypass in ["--allow-no-manifest", "--skip-manifest"] {
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
        let doc: toml::Value = toml::from_str(include_str!("../.cli-flags.toml"))
            .expect(".cli-flags.toml must be valid TOML");

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
                        env.starts_with("ZED_PKG_"),
                        "flag --{} env `{env}` must use the ZED_PKG_ prefix",
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
