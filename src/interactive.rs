//! Opt-in, fail-closed confirmations for mutating lifecycle steps.
//!
//! `--interactive` is deliberately global: command implementations can place
//! checkpoints immediately before the mutation they own instead of relying on
//! one coarse prompt in `main`. Redirected input or diagnostics never count as
//! consent, and CI or a dumb terminal disables prompting.

use std::io::{self, BufRead, Write};

use anyhow::{Result, bail};

/// Confirm one mutation when interactive mode is enabled.
///
/// A negative answer, EOF, or a process context that cannot safely prompt
/// aborts before the caller performs the described step. Non-interactive mode
/// remains automation-safe and never reads stdin.
pub fn confirm(enabled: bool, step: &str) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    confirm_with(
        enabled,
        crate::terminal_context::current().can_prompt,
        step,
        &mut input,
        &mut output,
    )
}

fn confirm_with<R: BufRead, W: Write>(
    enabled: bool,
    can_prompt: bool,
    step: &str,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    if !can_prompt {
        bail!(
            "--interactive requires terminal stdin and stderr outside CI or a dumb terminal; `{step}` was not started"
        );
    }
    write!(output, "interactive: {step}? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        bail!("interactive confirmation closed; `{step}` was not started");
    }
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("interactive confirmation declined; `{step}` was not started")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn disabled_mode_never_reads_and_yes_is_explicit() {
        confirm_with(
            false,
            false,
            "mutate",
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap();

        for accepted in [b"y\n".as_slice(), b" YES \n".as_slice()] {
            confirm_with(
                true,
                true,
                "mutate",
                &mut Cursor::new(accepted),
                &mut Vec::new(),
            )
            .unwrap();
        }
        for rejected in [b"\n".as_slice(), b"no\n".as_slice(), b"maybe\n".as_slice()] {
            assert!(
                confirm_with(
                    true,
                    true,
                    "mutate",
                    &mut Cursor::new(rejected),
                    &mut Vec::new(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn unsafe_context_and_eof_fail_closed() {
        let redirected = confirm_with(
            true,
            false,
            "publish",
            &mut Cursor::new(b"yes\n"),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(redirected.contains("terminal stdin and stderr"));

        let eof = confirm_with(
            true,
            true,
            "publish",
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(eof.contains("closed"));
    }
}
