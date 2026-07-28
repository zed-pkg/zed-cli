#!/usr/bin/env python3
"""Finish DEN-567 after the manifestless installer is materialized."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, content: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")


def replace_once(path: str, old: str, new: str, label: str) -> None:
    content = read(path)
    if new in content:
        return
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: {label}: expected one target, found {count}")
    write(path, content.replace(old, new, 1))


replace_once(
    "Cargo.toml",
    'flags2env = { git = "https://github.com/ORESoftware/flags-2-env.git", rev = "450031f54468d4fd054131effb6b5f300d3d1310" }',
    'flags2env = { git = "https://github.com/ORESoftware/flags-2-env.git", rev = "9483b92c1fb259f598858fdd2bef66417d87fb2c" }',
    "pin the merged reviewed flags2env revision",
)

replace_once(
    "src/cli.rs",
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthProvider {''',
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthProvider {''',
    "add supported completion shells",
)
replace_once(
    "src/cli.rs",
    '''    Init {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: Option<String>,
        #[arg(long, env = "ZED_PKG_NAME")]
        name: Option<String>,
    },
    /// Add a dependency''',
    '''    Init {
        #[arg(long, env = "ZED_PKG_ORG")]
        org: Option<String>,
        #[arg(long, env = "ZED_PKG_NAME")]
        name: Option<String>,
    },
    /// Generate completion from the same Clap command model used at runtime
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Add a dependency''',
    "add the completions command",
)

replace_once(
    "src/main.rs",
    '''use zed_cli::config::Config;
use zed_cli::ops;''',
    '''use zed_cli::completion;
use zed_cli::config::Config;
use zed_cli::ops;''',
    "import completion dispatch",
)
replace_once(
    "src/main.rs",
    '''fn main() {
    zed_cli::flags::apply_cli_flags();
    let cli = Cli::parse();''',
    '''fn main() {
    if let Err(error) = zed_cli::flags::apply_cli_flags() {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
    let cli = Cli::parse();''',
    "fail closed when the embedded flag contract rejects startup",
)
replace_once(
    "src/main.rs",
    '''        Cmd::Init { org, name } => ops::init(&cwd, org, name),
        Cmd::Add { spec } => ops::add(&cwd, &cfg, &spec),''',
    '''        Cmd::Init { org, name } => ops::init(&cwd, org, name),
        Cmd::Completions { shell } => {
            completion::print(shell);
            Ok(())
        }
        Cmd::Add { spec } => ops::add(&cwd, &cfg, &spec),''',
    "dispatch completion generation",
)

COMPLETION_RS = r'''//! Shell-completion generation from the same Clap command model used at runtime.

use std::io;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::{Cli, CompletionShell};

fn generator(shell: CompletionShell) -> Shell {
    match shell {
        CompletionShell::Bash => Shell::Bash,
        CompletionShell::Zsh => Shell::Zsh,
    }
}

/// Write a static completion script for `zed` to stdout.
pub fn print(shell: CompletionShell) {
    let mut command = Cli::command();
    generate(generator(shell), &mut command, "zed", &mut io::stdout());
}

#[cfg(test)]
fn render(shell: CompletionShell) -> String {
    let mut command = Cli::command();
    let mut output = Vec::new();
    generate(generator(shell), &mut command, "zed", &mut output);
    String::from_utf8(output).expect("completion output must be UTF-8")
}

#[cfg(test)]
mod tests {
    use crate::cli::CompletionShell;

    use super::render;

    fn assert_contract(script: &str) {
        for command in ["install", "init", "completions", "self-update", "r2g"] {
            assert!(script.contains(command), "missing command {command:?}");
        }
        for option in ["--allow-no-manifest", "--skip-manifest", "--install-mode"] {
            assert!(script.contains(option), "missing option {option:?}");
        }
    }

    #[test]
    fn bash_completion_contains_commands_aliases_and_manifestless_flags() {
        let script = render(CompletionShell::Bash);
        assert!(script.contains("_zed"), "missing generated completion function");
        assert!(script.contains("complete"), "missing Bash completion registration");
        assert_contract(&script);
    }

    #[test]
    fn zsh_completion_contains_commands_aliases_and_manifestless_flags() {
        let script = render(CompletionShell::Zsh);
        assert!(script.contains("#compdef zed"), "missing Zsh compdef header");
        assert!(script.contains("_zed"), "missing generated completion function");
        assert_contract(&script);
    }
}
'''
write("src/completion.rs", COMPLETION_RS)

replace_once(
    ".cli-flags.toml",
    '''[commands.init]
help = "Create a package manifest."

[commands.add]''',
    '''[commands.init]
help = "Create a package manifest."

[commands.completions]
help = "Generate Bash or Zsh completion from the runtime command model."
allow_unknown = true

[commands.add]''',
    "declare completion generation in the embedded flag contract",
)

replace_once(
    "README.md",
    '''| `zed init` | Write a `.zpkg.toml` template |
| `zed add <org>/<name>[@req]` | Add a dependency and install |''',
    '''| `zed init` | Write a `.zpkg.toml` template |
| `zed completions bash\|zsh` | Generate shell completion from the runtime Clap command model |
| `zed add <org>/<name>[@req]` | Add a dependency and install |''',
    "document the completion command",
)
replace_once(
    "README.md",
    '''### Installing without a Zed manifest
''',
    '''### Shell completion

Generate completion directly from the installed binary so aliases, commands,
and options cannot drift from the runtime parser:

```sh
# Bash
mkdir -p ~/.local/share/bash-completion/completions
zed completions bash > ~/.local/share/bash-completion/completions/zed

# Zsh
mkdir -p ~/.zfunc
zed completions zsh > ~/.zfunc/_zed
# add `fpath=(~/.zfunc $fpath)` before `compinit` in ~/.zshrc
```

The GitHub Actions gate syntax-checks both scripts, registers the Bash function
with programmable-completion builtins, and loads the Zsh function through
`compinit`/`compdef`.

### Installing without a Zed manifest
''',
    "add shell activation documentation",
)

ci = read(".github/workflows/ci.yml")
if "  shell-completions:" not in ci:
    ci += r'''

  shell-completions:
    name: Bash and Zsh completion contracts
    runs-on: ubuntu-24.04
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
        with:
          path: zed-cli
          persist-credentials: false
          show-progress: false
      - uses: actions/checkout@v4
        with:
          repository: zed-pkg/zed-interfaces
          path: zed-interfaces
          persist-credentials: false
          show-progress: false
      - uses: dtolnay/rust-toolchain@stable
      - name: Install Zsh
        run: |
          sudo apt-get update
          sudo apt-get install --yes --no-install-recommends zsh
      - name: Build the real binary with the immutable parser lock
        working-directory: zed-cli
        run: |
          set -euo pipefail
          grep -F 'rev = "9483b92c1fb259f598858fdd2bef66417d87fb2c"' Cargo.toml
          grep -F '9483b92c1fb259f598858fdd2bef66417d87fb2c' Cargo.lock
          cargo build --locked --bin zed
      - name: Generate and execute Bash completion
        working-directory: zed-cli
        run: |
          set -euo pipefail
          ./target/debug/zed completions bash > "$RUNNER_TEMP/zed.bash"
          bash -n "$RUNNER_TEMP/zed.bash"
          bash --noprofile --norc -c '
            set -euo pipefail
            source "$1"
            complete -p zed | grep -F "complete -F _zed zed"
            COMP_WORDS=(zed install --)
            COMP_CWORD=2
            COMPREPLY=()
            _zed
            printf "%s\n" "${COMPREPLY[@]}" | grep -Fx -- --allow-no-manifest
            printf "%s\n" "${COMPREPLY[@]}" | grep -Fx -- --skip-manifest
          ' _ "$RUNNER_TEMP/zed.bash"
      - name: Generate and register Zsh completion
        working-directory: zed-cli
        run: |
          set -euo pipefail
          ./target/debug/zed completions zsh > "$RUNNER_TEMP/_zed"
          zsh -n "$RUNNER_TEMP/_zed"
          grep -F -- '--allow-no-manifest' "$RUNNER_TEMP/_zed"
          grep -F -- '--skip-manifest' "$RUNNER_TEMP/_zed"
          grep -F -- 'completions' "$RUNNER_TEMP/_zed"
          zsh -f -c '
            set -e
            autoload -Uz compinit
            compinit -d "$2"
            source "$1"
            whence -w _zed | grep -F "_zed: function"
            compdef _zed zed
            [[ "${_comps[zed]}" == _zed ]]
          ' _ "$RUNNER_TEMP/_zed" "$RUNNER_TEMP/.zcompdump"
'''
    write(".github/workflows/ci.yml", ci)

print("DEN-567 completion/startup wiring applied")
