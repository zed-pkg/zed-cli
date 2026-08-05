//! Conservative process I/O, terminal, and shell detection.
//!
//! Detection is observational. Command implementations must opt in to richer
//! behavior; this module never changes a command's stdout format on its own.
//! At process startup [`publish_process_environment`] exposes the snapshot to
//! child processes through the reserved `ZED_PKG_CONTEXT_*` namespace.

use std::collections::BTreeMap;
use std::env;
use std::io::{self, IsTerminal};
use std::sync::OnceLock;

/// Version of the environment contract published for child processes.
pub const CONTEXT_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Plain,
    Machine,
}

impl OutputMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Plain => "plain",
            Self::Machine => "machine",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    Bash,
    Zsh,
    Fish,
    Nushell,
    PowerShell,
    Cmd,
    Sh,
    Unknown,
}

impl ShellFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Nushell => "nushell",
            Self::PowerShell => "powershell",
            Self::Cmd => "cmd",
            Self::Sh => "sh",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSource {
    Override,
    ShellEnvironment,
    PowerShellEnvironment,
    ComSpec,
    ParentContext,
    Unknown,
}

impl ShellSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::ShellEnvironment => "shell-env",
            Self::PowerShellEnvironment => "powershell-env",
            Self::ComSpec => "comspec",
            Self::ParentContext => "parent-context",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalFamily {
    VsCode,
    WezTerm,
    ITerm,
    Ghostty,
    AppleTerminal,
    WindowsTerminal,
    Kitty,
    Tmux,
    Screen,
    Unknown,
}

impl TerminalFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VsCode => "vscode",
            Self::WezTerm => "wezterm",
            Self::ITerm => "iterm",
            Self::Ghostty => "ghostty",
            Self::AppleTerminal => "apple-terminal",
            Self::WindowsTerminal => "windows-terminal",
            Self::Kitty => "kitty",
            Self::Tmux => "tmux",
            Self::Screen => "screen",
            Self::Unknown => "unknown",
        }
    }
}

/// One immutable snapshot of process-facing terminal capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalContext {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    pub interactive: bool,
    pub can_prompt: bool,
    pub ci: bool,
    pub dumb: bool,
    pub nested: bool,
    pub output_mode: OutputMode,
    pub shell: ShellFamily,
    pub shell_source: ShellSource,
    pub terminal: TerminalFamily,
    pub color_stdout: bool,
    pub color_stderr: bool,
    pub unicode: bool,
    pub hyperlinks: bool,
    pub columns: Option<u16>,
}

/// Return the process snapshot. It is computed once so all command paths make
/// decisions from one consistent view of the process environment.
pub fn current() -> &'static TerminalContext {
    static CONTEXT: OnceLock<TerminalContext> = OnceLock::new();
    CONTEXT.get_or_init(|| Probe::from_process().detect())
}

/// Publish the current snapshot for subprocesses without changing the public
/// stdout/stderr contract of the current command.
///
/// This must run during single-threaded process startup. The namespace is
/// reserved and intentionally overwritten so inherited values cannot describe
/// the wrong file descriptors after a pipeline or redirection boundary.
pub fn publish_process_environment() {
    let context = current();
    for (key, value) in context.environment() {
        // SAFETY: `main` calls this during startup, before workers are created.
        unsafe { env::set_var(key, value) };
    }
}

impl TerminalContext {
    fn environment(self) -> [(&'static str, String); 18] {
        [
            ("ZED_PKG_CONTEXT_VERSION", CONTEXT_VERSION.to_string()),
            (
                "ZED_PKG_CONTEXT_STDIN_TTY",
                bool_text(self.stdin_tty).into(),
            ),
            (
                "ZED_PKG_CONTEXT_STDOUT_TTY",
                bool_text(self.stdout_tty).into(),
            ),
            (
                "ZED_PKG_CONTEXT_STDERR_TTY",
                bool_text(self.stderr_tty).into(),
            ),
            (
                "ZED_PKG_CONTEXT_INTERACTIVE",
                bool_text(self.interactive).into(),
            ),
            (
                "ZED_PKG_CONTEXT_CAN_PROMPT",
                bool_text(self.can_prompt).into(),
            ),
            ("ZED_PKG_CONTEXT_CI", bool_text(self.ci).into()),
            ("ZED_PKG_CONTEXT_DUMB", bool_text(self.dumb).into()),
            ("ZED_PKG_CONTEXT_NESTED", bool_text(self.nested).into()),
            (
                "ZED_PKG_CONTEXT_OUTPUT_MODE",
                self.output_mode.as_str().into(),
            ),
            ("ZED_PKG_CONTEXT_SHELL", self.shell.as_str().into()),
            (
                "ZED_PKG_CONTEXT_SHELL_SOURCE",
                self.shell_source.as_str().into(),
            ),
            ("ZED_PKG_CONTEXT_TERMINAL", self.terminal.as_str().into()),
            (
                "ZED_PKG_CONTEXT_COLOR_STDOUT",
                bool_text(self.color_stdout).into(),
            ),
            (
                "ZED_PKG_CONTEXT_COLOR_STDERR",
                bool_text(self.color_stderr).into(),
            ),
            ("ZED_PKG_CONTEXT_UNICODE", bool_text(self.unicode).into()),
            (
                "ZED_PKG_CONTEXT_HYPERLINKS",
                bool_text(self.hyperlinks).into(),
            ),
            (
                "ZED_PKG_CONTEXT_COLUMNS",
                self.columns.unwrap_or_default().to_string(),
            ),
        ]
    }
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

#[derive(Debug)]
struct Probe {
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
    env: BTreeMap<String, String>,
}

impl Probe {
    fn from_process() -> Self {
        Self {
            stdin_tty: io::stdin().is_terminal(),
            stdout_tty: io::stdout().is_terminal(),
            stderr_tty: io::stderr().is_terminal(),
            env: env::vars_os()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        }
    }

    fn detect(&self) -> TerminalContext {
        let stdin_tty = self.forced_bool(
            &["ZED_PKG_FORCE_STDIN_TTY", "F2E_FORCE_STDIN_TTY"],
            self.stdin_tty,
        );
        let stdout_tty = self.forced_bool(
            &["ZED_PKG_FORCE_STDOUT_TTY", "F2E_FORCE_STDOUT_TTY"],
            self.stdout_tty,
        );
        let stderr_tty = self.forced_bool(
            &["ZED_PKG_FORCE_STDERR_TTY", "F2E_FORCE_STDERR_TTY"],
            self.stderr_tty,
        );
        let ci = self.forced_bool(&["ZED_PKG_FORCE_CI", "F2E_FORCE_CI"], self.detect_ci());
        let dumb = self
            .get("TERM")
            .is_some_and(|value| value.eq_ignore_ascii_case("dumb"));
        let can_prompt = stdin_tty && stderr_tty && !ci && !dumb;
        let output_mode = if stdout_tty && !ci && !dumb {
            OutputMode::Human
        } else if stdout_tty || stderr_tty {
            OutputMode::Plain
        } else {
            OutputMode::Machine
        };
        let (shell, shell_source) = self.detect_shell();
        let terminal = self.detect_terminal();
        let color_stdout = self.color_enabled(stdout_tty, ci, dumb);
        let color_stderr = self.color_enabled(stderr_tty, ci, dumb);
        let unicode = self.forced_bool(
            &["ZED_PKG_FORCE_UNICODE", "F2E_FORCE_UNICODE"],
            self.detect_unicode(),
        );
        let hyperlinks = stderr_tty && !ci && !dumb && self.detect_hyperlinks();
        let columns = self
            .get("COLUMNS")
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|columns| (20..=10_000).contains(columns));
        let nested = self.has("ZED_PKG_CONTEXT_VERSION") || self.has("F2E_CONTEXT_VERSION");

        TerminalContext {
            stdin_tty,
            stdout_tty,
            stderr_tty,
            interactive: can_prompt,
            can_prompt,
            ci,
            dumb,
            nested,
            output_mode,
            shell,
            shell_source,
            terminal,
            color_stdout,
            color_stderr,
            unicode,
            hyperlinks,
            columns,
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    fn has(&self, key: &str) -> bool {
        self.env.contains_key(key)
    }

    fn first(&self, keys: &[&str]) -> Option<&str> {
        keys.iter()
            .filter_map(|key| self.get(key))
            .find(|value| !value.is_empty())
    }

    fn forced_bool(&self, keys: &[&str], detected: bool) -> bool {
        let Some(value) = self.first(keys) else {
            return detected;
        };
        if value.eq_ignore_ascii_case("auto") {
            detected
        } else {
            value_truthy(value)
        }
    }

    fn detect_ci(&self) -> bool {
        if self.get("CI").is_some_and(value_truthy) {
            return true;
        }
        [
            "GITHUB_ACTIONS",
            "GITLAB_CI",
            "BUILDKITE",
            "CIRCLECI",
            "TF_BUILD",
            "JENKINS_URL",
            "TEAMCITY_VERSION",
            "BUILD_BUILDID",
        ]
        .iter()
        .any(|marker| self.has(marker))
    }

    fn detect_shell(&self) -> (ShellFamily, ShellSource) {
        if let Some(shell) = self.first(&["ZED_PKG_SHELL", "F2E_SHELL"]) {
            return (classify_shell(shell), ShellSource::Override);
        }
        if let Some(shell) = self.get("SHELL").filter(|value| !value.is_empty()) {
            return (classify_shell(shell), ShellSource::ShellEnvironment);
        }
        if self.has("POWERSHELL_DISTRIBUTION_CHANNEL") || self.has("PSModulePath") {
            return (ShellFamily::PowerShell, ShellSource::PowerShellEnvironment);
        }
        if let Some(shell) = self.get("COMSPEC").filter(|value| !value.is_empty()) {
            return (classify_shell(shell), ShellSource::ComSpec);
        }
        if let Some(shell) = self.first(&["ZED_PKG_CONTEXT_SHELL", "F2E_CONTEXT_SHELL"]) {
            return (classify_shell(shell), ShellSource::ParentContext);
        }
        (ShellFamily::Unknown, ShellSource::Unknown)
    }

    fn detect_terminal(&self) -> TerminalFamily {
        if let Some(program) = self.get("TERM_PROGRAM") {
            if contains_ci(program, "vscode") {
                return TerminalFamily::VsCode;
            }
            if contains_ci(program, "wezterm") {
                return TerminalFamily::WezTerm;
            }
            if contains_ci(program, "iterm") {
                return TerminalFamily::ITerm;
            }
            if contains_ci(program, "ghostty") {
                return TerminalFamily::Ghostty;
            }
            if contains_ci(program, "apple_terminal") {
                return TerminalFamily::AppleTerminal;
            }
        }
        if self.has("WT_SESSION") {
            return TerminalFamily::WindowsTerminal;
        }
        let term = self.get("TERM").unwrap_or_default();
        if self.has("TMUX") || contains_ci(term, "tmux") {
            return TerminalFamily::Tmux;
        }
        if contains_ci(term, "kitty") {
            return TerminalFamily::Kitty;
        }
        if contains_ci(term, "screen") {
            return TerminalFamily::Screen;
        }
        TerminalFamily::Unknown
    }

    fn color_enabled(&self, tty: bool, ci: bool, dumb: bool) -> bool {
        if let Some(value) = self.first(&["ZED_PKG_FORCE_COLOR", "F2E_FORCE_COLOR"])
            && !value.eq_ignore_ascii_case("auto")
        {
            return value_truthy(value);
        }
        if self.has("NO_COLOR") {
            return false;
        }
        if self.get("FORCE_COLOR").is_some_and(value_truthy)
            || self.get("CLICOLOR_FORCE").is_some_and(value_truthy)
        {
            return true;
        }
        if self
            .get("CLICOLOR")
            .is_some_and(|value| value == "0" || value.eq_ignore_ascii_case("false"))
        {
            return false;
        }
        tty && !ci && !dumb
    }

    fn detect_unicode(&self) -> bool {
        if self.has("WT_SESSION") {
            return true;
        }
        self.first(&["LC_ALL", "LC_CTYPE", "LANG"])
            .is_some_and(|locale| contains_ci(locale, "utf-8") || contains_ci(locale, "utf8"))
    }

    fn detect_hyperlinks(&self) -> bool {
        if self.has("WT_SESSION") {
            return true;
        }
        if self.get("TERM_PROGRAM").is_some_and(|program| {
            ["iterm", "wezterm", "vscode", "ghostty"]
                .iter()
                .any(|name| contains_ci(program, name))
        }) {
            return true;
        }
        if self
            .get("TERM")
            .is_some_and(|term| contains_ci(term, "kitty"))
        {
            return true;
        }
        self.get("VTE_VERSION")
            .and_then(|value| value.parse::<u32>().ok())
            .is_some_and(|version| version >= 5000)
    }
}

fn value_truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off" | "never"
    )
}

fn contains_ci(value: &str, needle: &str) -> bool {
    value
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn classify_shell(value: &str) -> ShellFamily {
    let base = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
    match base.as_str() {
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe" => ShellFamily::PowerShell,
        "nu" | "nu.exe" | "nushell" => ShellFamily::Nushell,
        "cmd" | "cmd.exe" => ShellFamily::Cmd,
        "sh" | "sh.exe" => ShellFamily::Sh,
        _ if base.contains("fish") => ShellFamily::Fish,
        _ if base.contains("zsh") => ShellFamily::Zsh,
        _ if base.contains("bash") => ShellFamily::Bash,
        _ if ["dash", "ksh", "ash"]
            .iter()
            .any(|shell| base.contains(shell)) =>
        {
            ShellFamily::Sh
        }
        _ => ShellFamily::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(entries: &[(&str, &str)]) -> Probe {
        Probe {
            stdin_tty: false,
            stdout_tty: false,
            stderr_tty: false,
            env: entries
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        }
    }

    #[test]
    fn terminal_prompting_requires_input_and_diagnostic_ttys() {
        let mut probe = probe(&[("SHELL", "/bin/zsh"), ("LANG", "en_US.UTF-8")]);
        probe.stdin_tty = true;
        probe.stderr_tty = true;
        let context = probe.detect();
        assert!(context.can_prompt);
        assert_eq!(context.shell, ShellFamily::Zsh);
        assert_eq!(context.shell_source, ShellSource::ShellEnvironment);
        assert!(context.unicode);

        probe.env.insert("CI".into(), "true".into());
        assert!(!probe.detect().can_prompt);
    }

    #[test]
    fn redirection_produces_plain_or_machine_output_without_changing_data_shape() {
        let mut stderr_only = probe(&[]);
        stderr_only.stderr_tty = true;
        assert_eq!(stderr_only.detect().output_mode, OutputMode::Plain);
        assert_eq!(probe(&[]).detect().output_mode, OutputMode::Machine);
    }

    #[test]
    fn explicit_overrides_are_deterministic_and_parent_context_is_fallback_only() {
        let context = probe(&[
            ("F2E_SHELL", "/usr/bin/fish"),
            ("F2E_FORCE_STDIN_TTY", "1"),
            ("F2E_FORCE_STDERR_TTY", "1"),
            ("F2E_FORCE_CI", "0"),
            ("F2E_FORCE_COLOR", "1"),
            ("COLUMNS", "132"),
            ("ZED_PKG_CONTEXT_VERSION", "1"),
        ])
        .detect();
        assert_eq!(context.shell, ShellFamily::Fish);
        assert_eq!(context.shell_source, ShellSource::Override);
        assert!(context.can_prompt);
        assert!(context.color_stdout);
        assert_eq!(context.columns, Some(132));
        assert!(context.nested);

        let inherited = probe(&[("F2E_CONTEXT_SHELL", "powershell")]).detect();
        assert_eq!(inherited.shell, ShellFamily::PowerShell);
        assert_eq!(inherited.shell_source, ShellSource::ParentContext);
    }

    #[test]
    fn no_color_wins_unless_the_application_force_override_is_explicit() {
        let mut terminal = probe(&[("NO_COLOR", "")]);
        terminal.stdout_tty = true;
        assert!(!terminal.detect().color_stdout);

        terminal
            .env
            .insert("ZED_PKG_FORCE_COLOR".into(), "1".into());
        assert!(terminal.detect().color_stdout);
    }

    #[test]
    fn shell_classifier_handles_paths_and_common_families() {
        assert_eq!(classify_shell("/opt/homebrew/bin/bash"), ShellFamily::Bash);
        assert_eq!(
            classify_shell("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
            ShellFamily::PowerShell
        );
        assert_eq!(classify_shell("nu"), ShellFamily::Nushell);
        assert_eq!(classify_shell("dash"), ShellFamily::Sh);
        assert_eq!(classify_shell("custom-shell"), ShellFamily::Unknown);
    }
}
