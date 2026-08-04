# Global executable packages

Zed keeps project dependencies project-local by default. A command-line tool is different: its executable should normally be available from the user's `PATH`, independent of any one repository.

Use the explicit global package namespace:

```sh
zed global install acme/tool
zed global install acme/tool@^2
```

The npm/cargo-style spelling is an exact compatibility route to the same implementation:

```sh
zed install --global acme/tool
```

## Storage model

Each requested top-level package gets an isolated profile:

```text
$ZED_PKG_HOME/global/profiles/<org>/<name>/
├── .zpkg.lock
├── .zed-global-profile.json
└── zed_modules/
    ├── <org>/<name>/
    └── .bin/<executable>
```

A profile resolves and locks its own complete dependency graph. Two global tools may therefore use incompatible versions of the same transitive package without forcing one global dependency solution.

The package artifact still lives once in Zed's content-addressed store. A profile normally symlinks package trees from that store, while exposed executables are independent, executable copies owned by the global installer.

## PATH directory

On Unix-like systems, executables are copied to `~/.local/bin` by default. On Windows, Zed uses a per-user directory below the local application-data root. Print the exact path with:

```sh
zed global bin-dir
```

Override it for one invocation or permanently:

```sh
zed --global-bin-dir "$HOME/bin" global install acme/tool
export ZED_PKG_GLOBAL_BIN_DIR="$HOME/bin"
```

Zed reports when the selected directory is not on `PATH`; it never silently edits shell startup files or the Windows user environment.

## Builds and executable declarations

A package exposes commands through its root manifest:

```toml
[build]
command = "cargo build --release --locked"
outputs = ["target/release/acme"]

[bin]
acme = "target/release/acme"
```

Package build hooks execute author-supplied code and remain opt-in:

```sh
zed global install acme/tool --allow-build
```

Prebuilt packages do not need a build hook. Their `[bin]` values may point directly at executable files already present in the artifact.

## Ownership and collisions

Zed records the package owner and SHA-256 hash of every executable it places in the global bin directory.

- An unrelated existing command is never overwritten.
- Two installed packages exposing the same command name fail closed.
- Uninstall removes an executable only when its current bytes still match the version Zed installed.
- A command changed after installation is retained with a warning rather than deleted.

These rules make a shared directory such as `~/.local/bin` safe to use alongside manually installed tools and other package managers.

## Lifecycle

```sh
# Inspect profiles and their commands
zed global list

# Re-materialize every exact lock after moving machines or clearing the store
zed global install --frozen --allow-build

# Re-materialize one exact profile
zed global install --frozen acme/tool --allow-build

# Remove one profile and its still-owned commands
zed global uninstall acme/tool
zed uninstall --global acme/tool

# Remove all Zed-managed global profiles
zed global uninstall
```

The global lock serializes profile and PATH mutations across terminals. Package downloads and builds continue to use the normal store and build-cache locks.

## Installing Zed with Zed

The `zed-pkg/zed-cli` repository is itself a Zed package. Once a bootstrap `zed` binary is available, it can install or upgrade the CLI through the same contract:

```sh
zed global install zed-pkg/zed-cli --allow-build
```

The resulting `zed` executable is managed in the configured global bin directory; the package source and build output remain reproducibly locked in the isolated `zed-pkg/zed-cli` profile.
