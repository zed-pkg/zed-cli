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
- identifies a project root that owns the fixture manifest; and
- returns the child PowerShell exit code unchanged.

Using project ownership rather than textual path equality is intentional on Windows: equivalent paths may be rendered with normal drive-letter syntax or the Win32 verbatim `\\?\` prefix. The security assertion concerns the selected project and profile behavior, not one display spelling of the same directory.

The companion independent acceptance is tracked by [DEN-1614](https://linear.app/denman/issue/DEN-1614/zed-e2e-add-windows-clean-room-certification-for-zed-develop). The implementation correction is tracked by [DEN-1616](https://linear.app/denman/issue/DEN-1616/zed-cli-suppress-powershell-profiles-in-zed-develop-command-mode).
