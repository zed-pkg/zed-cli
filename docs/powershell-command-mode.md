# PowerShell command-mode trust boundary

`zed develop -c <command>` is the non-interactive automation boundary for developers, agents, and CI. On Windows, an explicitly selected `pwsh`, `pwsh.exe`, `powershell`, or `powershell.exe` is invoked as:

```text
<shell> -NoLogo -NoProfile -NonInteractive -Command <command>
```

The arguments are intentional:

- `-NoLogo` removes banner output from automation;
- `-NoProfile` prevents current-user, all-users, current-host, and all-host PowerShell profiles from executing implicitly;
- `-NonInteractive` prevents prompts and interactive host behavior;
- `-Command` executes the caller-supplied command through PowerShell's normal command parser.

This is the PowerShell equivalent of the existing `cmd.exe /D /S /C <command>` boundary, where `/D` disables AutoRun commands.

The no-profile rule applies only when `-c` / `--command` is present. An explicitly requested interactive PowerShell session is started without injected command-mode switches so the user retains PowerShell's native interactive startup semantics.

## Security invariant

PowerShell profiles may contain arbitrary code, environment mutations, credential lookups, network calls, or user-specific aliases. Non-interactive `zed develop` execution must therefore not depend on or execute those profiles unless a future explicit, trusted activation feature defines and obtains a separate consent decision.

The contract is checked at two independent layers:

1. cross-platform argument-vector tests keep PowerShell, cmd.exe, and generic shell dispatch synchronized; and
2. a native Windows regression executes the real `zed.exe` against actual PowerShell profile locations.

The Windows regression suite creates current-user profile files under a temporary `HOME` and `USERPROFILE`, proves the canary profile loads when PowerShell is started normally, and then proves the real `zed.exe` command path:

- does not execute or emit the profile canary;
- receives the managed `ZED_DEV` environment;
- starts at the selected `ZED_DEV_PROJECT_ROOT`; and
- returns the child PowerShell exit code unchanged.

## Canonical identity versus process current directory

`ZED_DEV_PROJECT_ROOT` retains the canonical filesystem identity used for project selection, caches, managed environment, and evidence. On Windows that identity may include a Win32 verbatim prefix:

```text
\\?\C:\path
\\?\UNC\server\share\path
```

Those forms are valid filesystem identities but are not accepted consistently as a child-process current directory. Before launching a Windows child, Zed converts only the prefix to the equivalent process-compatible spelling:

```text
\\?\C:\path                    -> C:\path
\\?\UNC\server\share\path     -> \\server\share\path
```

Ordinary drive paths, ordinary UNC paths, and device paths remain unchanged. The conversion operates on UTF-16 code units rather than lossy UTF-8 strings, so Unicode project names are preserved exactly. This separation is tracked by [DEN-1634](https://linear.app/denman/issue/DEN-1634/zed-cli-normalize-windows-child-process-cwd-for-verbatim-project-paths).

## Reviewed evidence stack

A change to this boundary is complete only when all of the following agree on the same immutable candidate:

1. the typed shell-argument unit tests;
2. the two shared cross-platform shell matrices;
3. the native Windows profile and child-current-directory regression; and
4. the independent Windows clean-room consumer contract in `zed-pkg/zed-e2e`.

The independent contract must pin the exact reviewed CLI commit, build the real `zed.exe`, and prove PowerShell and cmd.exe behavior without importing implementation test helpers. Temporary finalizers, cleanup commits, or a later moving branch tip are not validation evidence. A source change after a green run requires a new immutable pin and complete replay.

## Exact-head validation ownership

The profile correction, child-current-directory normalization, shared shell matrices, native Windows canary, and this trust-boundary note are reviewed as one immutable CLI candidate. The independent `zed-pkg/zed-e2e` contract pins that exact commit; it does not follow the branch or assume that a later `main` still represents the reviewed candidate. A candidate change requires a new explicit pin and a complete Windows replay.

The companion independent acceptance is tracked by [DEN-1614](https://linear.app/denman/issue/DEN-1614/zed-e2e-add-windows-clean-room-certification-for-zed-develop). The PowerShell correction is tracked by [DEN-1616](https://linear.app/denman/issue/DEN-1616/zed-cli-suppress-powershell-profiles-in-zed-develop-command-mode).
