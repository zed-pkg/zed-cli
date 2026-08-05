# Terminal and shell context

Zed takes one immutable process-context snapshot at startup. The detector is conservative: it reports capabilities and gates explicit prompts, but it does **not** silently change JSON, line-oriented output, exit codes, or other public command contracts.

## What is detected

The snapshot records whether stdin, stdout, and stderr are terminals; whether the process is running in CI or a `TERM=dumb` environment; whether a safe prompt is possible; human/plain/machine output mode; a best-effort shell family and the evidence used; terminal family; color, Unicode, and hyperlink capability; terminal columns when `COLUMNS` is valid; and whether this is a nested Zed/flags2env invocation.

A prompt is allowed only when stdin and stderr are terminals and the process is neither CI nor a dumb terminal. Stdout is deliberately not required because a command may pipe its data while keeping diagnostics and prompts on stderr.

## Shell detection is best effort

There is no portable, race-free way for a process to prove which interactive shell launched it. Zed therefore labels the source of its decision:

1. `ZED_PKG_SHELL` or `F2E_SHELL` explicit override
2. `SHELL`
3. PowerShell environment markers
4. `COMSPEC`
5. inherited `ZED_PKG_CONTEXT_SHELL` or `F2E_CONTEXT_SHELL`
6. `unknown`

Supported families are `bash`, `zsh`, `fish`, `nushell`, `powershell`, `cmd`, `sh`, and `unknown`. Commands that generate shell syntax should still accept an explicit shell argument when correctness depends on it.

## Child-process contract

During startup Zed overwrites the reserved `ZED_PKG_CONTEXT_*` namespace with the current snapshot, including:

- `VERSION`, `STDIN_TTY`, `STDOUT_TTY`, `STDERR_TTY`
- `INTERACTIVE`, `CAN_PROMPT`, `CI`, `DUMB`, `NESTED`
- `OUTPUT_MODE`, `SHELL`, `SHELL_SOURCE`, `TERMINAL`
- `COLOR_STDOUT`, `COLOR_STDERR`, `UNICODE`, `HYPERLINKS`, `COLUMNS`

The overwrite is intentional: inherited file-descriptor facts can become wrong after a pipe or redirection. Children may use these values as hints, but should re-detect their own descriptors when possible.

## Deterministic overrides

For tests and carefully controlled wrappers, Zed accepts its namespaced override first and then the shared flags2env spelling:

- `ZED_PKG_FORCE_STDIN_TTY` / `F2E_FORCE_STDIN_TTY`
- `ZED_PKG_FORCE_STDOUT_TTY` / `F2E_FORCE_STDOUT_TTY`
- `ZED_PKG_FORCE_STDERR_TTY` / `F2E_FORCE_STDERR_TTY`
- `ZED_PKG_FORCE_CI` / `F2E_FORCE_CI`
- `ZED_PKG_FORCE_COLOR` / `F2E_FORCE_COLOR`
- `ZED_PKG_FORCE_UNICODE` / `F2E_FORCE_UNICODE`

Values such as `1`, `true`, `yes`, and `on` enable; `0`, `false`, `no`, `off`, and `never` disable; `auto` restores detection. `NO_COLOR` disables color unless an application-specific force override is explicitly set.
