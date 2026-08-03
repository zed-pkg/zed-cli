from pathlib import Path

OLD_REV = "dc0e0a0620b9462817950b552d3d334a184b1cb1"
NEW_REV = "19e6d74d9f9ff92d549d2072793ebe1116a25d90"

for path in ["Cargo.toml", "Cargo.lock"]:
    file = Path(path)
    text = file.read_text()
    count = text.count(OLD_REV)
    if count == 0:
        raise SystemExit(f"missing old zed-interfaces revision in {path}")
    file.write_text(text.replace(OLD_REV, NEW_REV))

cli = Path("src/cli.rs")
text = cli.read_text()
enum_anchor = "#[derive(Debug, Subcommand)]\npub enum Cmd {"
manager_enum = '''#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EnvironmentManagerArg {
    /// Import or verify project-local mise configuration.
    Mise,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {'''
if enum_anchor not in text:
    raise SystemExit("Cmd enum anchor missing")
text = text.replace(enum_anchor, manager_enum, 1)

completions_anchor = '''    /// Generate a completion script from the same typed command model used at runtime
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },'''
env_variant = '''    /// Import or verify project-local developer-environment configuration.
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// Generate a completion script from the same typed command model used at runtime
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },'''
if completions_anchor not in text:
    raise SystemExit("Completions anchor missing")
text = text.replace(completions_anchor, env_variant, 1)

release_anchor = "#[derive(Debug, Subcommand)]\npub enum ReleaseCmd {"
env_commands = '''#[derive(Debug, Subcommand)]
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

#[derive(Debug, Subcommand)]
pub enum ReleaseCmd {'''
if release_anchor not in text:
    raise SystemExit("ReleaseCmd anchor missing")
text = text.replace(release_anchor, env_commands, 1)

test_anchor = '''    #[test]
    fn completion_shells_are_typed_positionals() {
        for shell in ["bash", "zsh"] {
            let cli = Cli::try_parse_from(["zed", "completions", shell]).unwrap();
            assert!(matches!(cli.cmd, Cmd::Completions { .. }));
        }
    }
'''
env_test = test_anchor + '''
    #[test]
    fn environment_import_and_verify_are_typed() {
        for action in ["import", "verify"] {
            let cli = Cli::try_parse_from([
                "zed",
                "env",
                action,
                "mise",
                "--config",
                "mise.toml",
                "--lock",
                "mise.lock",
                "--frozen",
                "--json",
            ])
            .unwrap();
            assert!(matches!(cli.cmd, Cmd::Env { .. }));
        }
    }
'''
if test_anchor not in text:
    raise SystemExit("completion test anchor missing")
cli.write_text(text.replace(test_anchor, env_test, 1))

main = Path("src/main.rs")
text = main.read_text()
old_import = "use zed_cli::cli::{AuthCmd, CacheCmd, Cli, Cmd, OrgCmd, ReleaseCmd, StoreCmd};"
new_import = "use zed_cli::cli::{AuthCmd, CacheCmd, Cli, Cmd, EnvCmd, OrgCmd, ReleaseCmd, StoreCmd};"
if old_import not in text:
    raise SystemExit("main cli import anchor missing")
text = text.replace(old_import, new_import, 1)
if "use zed_cli::dev;\n" not in text:
    raise SystemExit("main dev import anchor missing")
text = text.replace("use zed_cli::dev;\n", "use zed_cli::dev;\nuse zed_cli::environment;\n", 1)
release_match = '''        Cmd::Release { cmd } => match cmd {
            ReleaseCmd::Plan { json } => release::plan(&cwd, json),
            ReleaseCmd::Preflight => preflight::preflight(&cwd),
        },'''
env_match = '''        Cmd::Env { cmd } => match cmd {
            EnvCmd::Import {
                manager: _,
                config,
                lock,
                frozen,
                json,
            } => {
                let imported = environment::import_mise(
                    &cwd,
                    config.as_deref(),
                    lock.as_deref(),
                    frozen,
                )?;
                environment::print_import(&imported, json)
            }
            EnvCmd::Verify {
                manager: _,
                config,
                lock,
                frozen,
                json,
            } => {
                let imported = environment::import_mise(
                    &cwd,
                    config.as_deref(),
                    lock.as_deref(),
                    frozen,
                )?;
                environment::print_verification(&imported, json)
            }
        },
        Cmd::Release { cmd } => match cmd {
            ReleaseCmd::Plan { json } => release::plan(&cwd, json),
            ReleaseCmd::Preflight => preflight::preflight(&cwd),
        },'''
if release_match not in text:
    raise SystemExit("main release match anchor missing")
main.write_text(text.replace(release_match, env_match, 1))

flags = Path(".cli-flags.toml")
text = flags.read_text()
flag_anchor = '''[flags.password_stdin]
env = "ZED_PKG_AUTH_PASSWORD_STDIN"
aliases = ["password-stdin"]
type = "bool"
default = "false"
help = "Read one password line from stdin."
'''
env_flags = flag_anchor + '''
[flags.env_config]
env = "ZED_PKG_ENV_CONFIG"
aliases = ["config"]
type = "string"
help = "Project-local environment-manager config path."

[flags.env_lock]
env = "ZED_PKG_ENV_LOCK"
aliases = ["lock"]
type = "string"
help = "Project-local environment-manager lockfile path."

[flags.env_json]
env = "ZED_PKG_ENV_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit environment interoperability output as JSON."
'''
if flag_anchor not in text:
    raise SystemExit("flags insertion anchor missing")
text = text.replace(flag_anchor, env_flags, 1)
command_anchor = '''[commands.completions]
help = "Generate Bash or Zsh completion from the typed CLI model."
'''
env_command = '''[commands.env]
help = "Import and verify project-local developer environments."

[commands.env.commands.import]
help = "Import manager-native state as a normalized EnvironmentPlan."

[commands.env.commands.verify]
help = "Verify config/lock coverage and normalized environment identity."

[commands.completions]
help = "Generate Bash or Zsh completion from the typed CLI model."
'''
if command_anchor not in text:
    raise SystemExit("commands insertion anchor missing")
flags.write_text(text.replace(command_anchor, env_command, 1))

readme = Path("README.md")
text = readme.read_text()
row = '| `zed uninstall [<org>/<name> ...]` (`zed un`) | Transactionally remove all or selected materialized packages while retaining the manifest and lockfile for a frozen reinstall |'
replacement = row + '''
| `zed env import mise [--config PATH] [--lock PATH] [--frozen] [--json]` | Import the supported project-local mise tool/lock subset as the shared normalized `EnvironmentPlan`; never loads parent/global config or executes hooks |
| `zed env verify mise [--config PATH] [--lock PATH] --frozen [--json]` | Fail closed on missing lock coverage, drift, malformed checksums, unsupported semantics, or non-portable frozen state and report the stable plan digest |'''
if row not in text:
    raise SystemExit("README command row anchor missing")
readme.write_text(text.replace(row, replacement, 1))
