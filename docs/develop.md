# `zed develop` / `zed dev`

`zed develop` enters a project-aware shell whose mutable tool state is isolated
under `.zed/dev`. The short alias is `zed dev`. The same command can execute a
single shell expression for coding agents and CI:

```sh
zed dev -c 'cargo test --all-targets'
zed develop -c 'pnpm test'
```

The command is deliberately a **cross-language environment coordinator**, not a
replacement for Cargo, npm, pnpm, uv, Go modules, Flutter, mise, Nix, or Zed's
normal package resolver.

## Startup sequence

1. Select the owning project from `.zpkg.toml`, `.zpkg.lock`, a native language
   manifest, or an unambiguous nested project.
2. In `--nix auto` mode, compose with the nearest `.nix/flake.nix` or
   `flake.nix` by re-entering through `nix develop`. Use `--nix never` to stay
   native or `--nix required` to fail closed.
3. After any Nix re-entry, `--mise auto` composes with project-local `mise.toml`
   or `.mise.toml` through `mise exec`. Use `--mise never` to bypass mise or
   `--mise required` to require both project configuration and the executable.
4. Unless `--no-install` is present, restore packages through the normal Zed
   manifest/lockfile installer. A manifestless lock is restored frozen and no
   synthetic `.zpkg.toml` is written.
5. Reuse an existing `.venv`, or create `.zed/dev/python/venv` for a detected
   Python project. Control this with `--python-venv auto|never|required`,
   `--python`, and `--venv`.
6. Build the managed environment and either enter a real interactive shell,
   run `-c/--command`, or emit JSON with `--print-env`.

An interactive invocation requires a terminal. Automation should always use
`-c` or `--print-env`.

## Environment layout

The following state is repo-local and ignored by this repository's existing
`/.zed` gitignore rule:

| Tool family | Managed state |
|---|---|
| Zed packages | `zed_modules/.bin` (or the manifest's custom install dir) |
| Python | `.venv` when present, otherwise `.zed/dev/python/venv`; pip/uv caches under `.zed/dev/python/cache` |
| Rust | `CARGO_HOME=.zed/dev/cargo/home`, `CARGO_TARGET_DIR=.zed/dev/cargo/target`; the generated `.zed/cargo-paths.toml` adapter is copied into the managed Cargo home |
| Node | Corepack, npm, Yarn, pnpm cache/prefix paths under `.zed/dev/node` |
| Go | `GOBIN`, `GOPATH`, `GOMODCACHE`, and `GOCACHE` under `.zed/dev/go`; `.zed/go.work` becomes `GOWORK` |
| Dart/Flutter | `PUB_CACHE=.zed/dev/dart/pub-cache` |
| Java | `GRADLE_USER_HOME=.zed/dev/java/gradle`; `.zed/classpath` becomes `CLASSPATH` |
| Ruby | `GEM_HOME` and `GEM_PATH` under `.zed/dev/ruby/gems` |
| XDG caches | `.zed/dev/xdg/cache` and `.zed/dev/xdg/state` |

The user's existing `PATH` remains at the end of the managed path. No global
packages are installed.

By default, `HOME` is preserved so ordinary credentials and developer settings
continue to work. `--isolated-home` redirects `HOME`, `XDG_CONFIG_HOME`, and
`XDG_DATA_HOME` into `.zed/dev` too. Zed does not copy provider credentials or
production environment files into that directory.

## Nix, mise, and direnv

`--nix auto` wraps the Zed environment inside the nearest flake's dev shell.
The child invocation is marked and re-run with `--nix never`, preventing
recursion. Existing Nix variables are inherited; Zed's explicit managed
language/cache variables are then overlaid for the child shell.

`--mise auto` looks only for project-local `mise.toml` or `.mise.toml` from the
selected project up to its Git, Mercurial, or Jujutsu checkout boundary. It
re-enters through `mise exec -- zed develop ...`, never through a mise task, so
repository task names cannot recurse back into `zed dev`. The child is marked
and re-run with `--mise never` and `--nix never`.

Nix is evaluated first. When a repository intentionally declares both, the Nix
child preserves the requested mise mode and mise is layered inside the Nix
environment. Use `--nix never --mise auto|required` for the preferred non-Nix
mise path.

With `--frozen`, mise composition requires the adjacent `mise.lock`, enables
mise's locked mode, ignores
`.tool-versions`, and redirects mise global and system configuration into empty
project-local directories under `.zed/dev/mise`. This prevents user-global
runtime pins from changing a frozen agent or CI run while leaving ordinary
non-frozen developer composition compatible with standard mise behavior.

A shell already entered by `mise exec` is detected through mise's environment
diff marker and is not re-entered. `zed develop` does not require direnv and
does not source `.envrc` itself. A shell already entered through direnv or
`nix develop` is detected through `IN_NIX_SHELL` and is not re-entered. No
dotenv file, including production env files, is loaded automatically.

Precedence is:

1. explicit CLI flags;
2. existing `ZED_DEV_*` / `ZED_PKG_*` environment values;
3. flags-2-env contract defaults.

Inside the spawned process, Nix, mise, and inherited shell variables remain
available, then Zed overlays the documented project-local tool variables.

## AI tool profile

The AI profile is explicit:

```sh
zed dev --profile ai
zed dev --profile ai --isolated-home -c 'codex --version'
```

It adds `.zed/dev/profiles/ai/bin` to `PATH` and reports whether common coding
CLIs such as Codex, Claude Code, Gemini, Kimi, Qwen, or OpenCode are visible.
Declare those tools as normal Zed packages when available, or place controlled
project shims in the profile directory. Credentials are never embedded or
copied; pass them through the process environment or authenticate inside an
isolated home.

## Examples

### Rust

```sh
zed dev -c 'cargo test --all-targets'
```

### Node / pnpm

```sh
zed dev -c 'corepack enable && pnpm install --frozen-lockfile && pnpm test'
```

### Python / uv

```sh
zed dev --python-venv required -c 'uv sync && python -m pytest'
```

### Polyglot repository

Run from the owning app directory, or from a root containing exactly one clear
nested project:

```sh
cd apps/api
zed dev -c 'cargo test && python -m pytest'
```

Ambiguous monorepo roots are not guessed; enter the intended workspace first.

### mise-backed non-Nix repository

```sh
mise install
zed dev --nix never --mise required -c 'agent-check'
```

mise selects the pinned runtimes, environment, and tools; Zed restores
`.zpkg.lock`, exposes `zed_modules/.bin`, and owns the project-local mutable
language caches. For CI, commit `mise.lock` and add `--frozen`.

### Nix-backed repository

```sh
zed dev --nix required -c 'agent-check'
```

This is equivalent in spirit to `nix develop -c agent-check`, with Zed package
bins and repo-local language state composed into the Nix environment.

## Flag and environment pairs

Every long option has a flags-2-env fallback:

| Option | Environment |
|---|---|
| `-c`, `--command` | `ZED_DEV_COMMAND` |
| `--shell` | `ZED_DEV_SHELL` |
| `--nix` | `ZED_DEV_NIX` |
| `--mise` | `ZED_DEV_MISE` |
| `--profile` | `ZED_DEV_PROFILE` |
| `--no-install` | `ZED_DEV_NO_INSTALL` |
| `--frozen` | `ZED_PKG_FROZEN` |
| `--allow-build` | `ZED_PKG_ALLOW_BUILD` |
| `--isolated-home` | `ZED_DEV_ISOLATED_HOME` |
| `--print-env` | `ZED_DEV_PRINT_ENV` |
| `--python-venv` | `ZED_DEV_PYTHON_VENV` |
| `--python` | `ZED_DEV_PYTHON` |
| `--venv` | `ZED_DEV_VENV` |

Global registry/auth flags retain their existing `ZED_PKG_*` mappings. Boolean
environment values accept `true/false`, `1/0`, `yes/no`, and `on/off`.
