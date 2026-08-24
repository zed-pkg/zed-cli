# Zed project lifecycle hooks

Zed runs project-owned hooks around install, build, pack, publish, and uninstall. Dependency package install hooks keep their existing, separate consent and allow-list checks.

## Convention files

A phase can be a file in either `.zed/` or `.zpkg/`. The `hooks/` subdirectory and common shell extensions are also accepted:

```text
.zed/pre-install
.zed/post-install.sh
.zed/hooks/pre-build
.zpkg/post-build.sh
.zpkg/pre-pack
.zpkg/post-pack
.zpkg/pre-publish
.zpkg/post-publish
.zpkg/pre-uninstall
.zpkg/post-uninstall
```

The full phase vocabulary also reserves `pre-test` and `post-test` for test-command integration. When more than one convention file exists, Zed runs `.zed`, `.zed/hooks`, `.zpkg`, then `.zpkg/hooks`; within each directory it checks the extensionless name, `.sh`, `.bash`, `.ps1`, `.cmd`, and `.bat`.

On Unix, executable hooks run directly; non-executable shell files run with `sh`. PowerShell and command files use their native interpreters. Zed rejects symbolic-link hooks and convention files that resolve outside the project root.

## Operation mapping

The lifecycle facade applies the same phase boundaries to equivalent dependency operations:

| Zed operation | Before | After |
| --- | --- | --- |
| `add`, `install` | `pre-install` | `post-install` |
| `build` | `pre-build` | `post-build` |
| `pack` | `pre-pack` | `post-pack` |
| `publish` | `pre-publish` | `post-publish` |
| `remove`, `uninstall` | `pre-uninstall` | `post-uninstall` |

`pre-test` and `post-test` are recognized configuration phases but remain reserved until the CLI exposes its dedicated test operation. This keeps manifests forward-compatible without silently attaching test hooks to an unrelated command.

## `.zpkg.toml` additions and overrides

Explicit commands complement convention files by default:

```toml
[lifecycle.pre-build]
commands = [
  "cargo fmt --check",
  "dart analyze --fatal-warnings"
]
```

Choose ordering or replacement with `mode`:

```toml
[lifecycle.pre-publish]
mode = "prepend" # explicit commands, then convention files
commands = ["node scripts/check-generated.mjs"]

[lifecycle.post-publish]
mode = "replace" # explicit commands only
command = "node scripts/record-release.mjs"

[lifecycle.pre-install]
mode = "disable" # no convention or explicit hook
```

Accepted aliases are `supplement`/`complement` for `append` and `override` for `replace`. `disable` cannot be combined with commands.

A phase may specify a shell executable/prefix and non-secret environment additions:

```toml
[lifecycle.pre-build]
shell = "bash"
env = { ZED_CONTRACT_MODE = "strict" }
commands = ["./scripts/pre-build"]
```

## Runtime environment and failure behavior

Every hook runs from the canonical project root with:

- `ZED_LIFECYCLE_PHASE`
- `ZED_LIFECYCLE_HOOK_INDEX` and `ZED_LIFECYCLE_HOOK_TOTAL`
- `ZED_LIFECYCLE_SOURCE`
- `ZED_LIFECYCLE_DEPTH` and `ZED_LIFECYCLE_STACK`
- `ZED_PROJECT_ROOT` and its package-oriented alias `ZED_PKG_ROOT`
- `ZED_PACKAGE_MANIFEST`

A failing pre-hook prevents the operation. A post-hook runs only after a successful operation; if it fails, Zed returns an error even though the operation has completed. Recursive invocation of the same phase is skipped. Set `ZED_SKIP_LIFECYCLE=1` for an explicit emergency bypass.

Project lifecycle hooks are trusted code from the checked-out root repository. CI should execute them only from reviewed commits and should not expose write-scoped secrets to untrusted pull requests. `ZED_SKIP_LIFECYCLE` bypasses only these root-project phases; it does not weaken dependency install-hook consent or native-dependency permission checks.
