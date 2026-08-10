//! Shell-completion generation from the same clap command model used at runtime.

use std::io;

use anyhow::{Context, Result};
use clap_complete::{Shell, generate};

use crate::cli_model;
use crate::{
    dev, external_subcommands, fetch, git_submodules, global, nix_bundle_write, nix_export_plan,
};

/// Build the complete built-in command tree without external extensions.
/// The external dispatcher uses this model to guarantee that a `zed-*`
/// executable can never shadow a built-in name or alias.
pub(crate) fn built_in_root_command() -> clap::Command {
    global::augment_root_command(git_submodules::augment_root_command(
        nix_bundle_write::augment_root_command(nix_export_plan::augment_root_command(
            fetch::augment_root_command(dev::augment_root_command(cli_model::command())),
        )),
    ))
}

/// Build the complete public command tree shared by root help and completion
/// generation. Every modular or external command must compose here rather than
/// maintaining a second, partial root-help model.
pub fn root_command() -> clap::Command {
    external_subcommands::augment_root_command(built_in_root_command())
}

/// Print the complete top-level help tree.
pub fn print_root_help() -> Result<()> {
    let mut command = root_command();
    command.print_help().context("printing zed help")?;
    println!();
    Ok(())
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
    fn bash_completion_contains_commands_aliases_and_manifest_flags() {
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
            "interop",
            "nix",
            "plan",
            "export",
            "bundle",
            "write",
            "oci",
            "push",
            "develop",
            "dev",
            "overtake",
            "global",
            "bin-dir",
            "completions",
            "self-update",
            "r2g",
            "gitops",
            "validate",
        ] {
            assert!(script.contains(command), "missing command {command:?}");
        }
        for option in [
            "--do-not-write-new-manifest",
            "--allow-no-manifest",
            "--skip-manifest",
            "--git-submodules",
            "--manifest",
            "--lock",
            "--require-lock",
            "--install-mode",
            "--global-bin-dir",
            "--frozen",
            "--json",
            "--target",
            "--flake-lock",
            "--out",
            "--output",
            "--password-stdin",
            "--registry-config",
            "--python-venv",
            "--isolated-home",
            "--catalog",
            "--offline",
        ] {
            assert!(script.contains(option), "missing option {option:?}");
        }
    }

    #[test]
    fn zsh_completion_contains_registration_commands_and_manifest_flags() {
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
            "interop",
            "nix",
            "plan",
            "export",
            "bundle",
            "write",
            "oci",
            "push",
            "develop",
            "dev",
            "overtake",
            "global",
            "bin-dir",
            "completions",
            "self-update",
            "r2g",
            "gitops",
            "validate",
        ] {
            assert!(script.contains(command), "missing command {command:?}");
        }
        for option in [
            "--do-not-write-new-manifest",
            "--allow-no-manifest",
            "--skip-manifest",
            "--git-submodules",
            "--manifest",
            "--lock",
            "--require-lock",
            "--install-mode",
            "--global-bin-dir",
            "--frozen",
            "--json",
            "--target",
            "--flake-lock",
            "--out",
            "--output",
            "--password-stdin",
            "--registry-config",
            "--python-venv",
            "--isolated-home",
            "--catalog",
            "--offline",
        ] {
            assert!(script.contains(option), "missing option {option:?}");
        }
    }
}
