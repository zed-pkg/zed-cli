//! Shell-completion generation from the same clap command model used at runtime.

use std::io;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::Cli;
use crate::{dev, fetch};

fn root_command() -> clap::Command {
    fetch::augment_root_command(dev::augment_root_command(Cli::command()))
}

/// Write a static completion script for `zed` to stdout.
pub fn print(shell: Shell) {
    let mut command = root_command();
    generate(shell, &mut command, "zed", &mut io::stdout());
}

#[cfg(test)]
fn render(shell: Shell) -> String {
    let mut command = root_command();
    let mut output = Vec::new();
    generate(shell, &mut command, "zed", &mut output);
    String::from_utf8(output).expect("completion output must be UTF-8")
}

#[cfg(test)]
mod tests {
    use clap_complete::Shell;

    use super::render;

    #[test]
    fn bash_completion_contains_commands_aliases_and_manifestless_flags() {
        let script = render(Shell::Bash);
        assert!(
            script.contains("_zed"),
            "missing generated completion function"
        );
        assert!(
            script.contains("complete"),
            "missing Bash completion registration"
        );
        for command in [
            "install",
            "init",
            "fetch",
            "develop",
            "dev",
            "completions",
            "self-update",
            "r2g",
        ] {
            assert!(script.contains(command), "missing command {command:?}");
        }
        for option in [
            "--allow-no-manifest",
            "--skip-manifest",
            "--install-mode",
            "--frozen",
            "--output",
            "--python-venv",
            "--isolated-home",
        ] {
            assert!(script.contains(option), "missing option {option:?}");
        }
    }

    #[test]
    fn zsh_completion_contains_registration_commands_and_manifestless_flags() {
        let script = render(Shell::Zsh);
        assert!(
            script.contains("#compdef zed"),
            "missing zsh compdef header"
        );
        assert!(script.contains("_zed"), "missing zsh completion function");
        for command in [
            "install",
            "init",
            "fetch",
            "develop",
            "dev",
            "completions",
            "self-update",
            "r2g",
        ] {
            assert!(script.contains(command), "missing command {command:?}");
        }
        for option in [
            "--allow-no-manifest",
            "--skip-manifest",
            "--install-mode",
            "--frozen",
            "--output",
            "--python-venv",
            "--isolated-home",
        ] {
            assert!(script.contains(option), "missing option {option:?}");
        }
    }
}
