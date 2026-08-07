# Native dependencies and install hooks

A Zed package may declare host-native prerequisites and package-local lifecycle hooks in `.zpkg.toml`.

```toml
[native-dependencies]
apt = ["pkg-config", "libssl-dev"]
apk = ["pkgconf", "openssl-dev"]
brew = ["pkg-config", "openssl@3"]
nix = ["pkg-config", "openssl"]

[hooks]
pre-install = ["./scripts/pre-install.sh"]
post-install = ["./scripts/post-install.sh"]
```

Polyglot targets may append target-specific declarations through `[targets.<target>.native-dependencies]` and `[targets.<target>.hooks]`. Package-level entries run first and target package lists are de-duplicated in declaration order.

## Explicit consent

Native package installation and lifecycle hooks are independent trust decisions:

```console
zed install \
  --allow-native-deps \
  --allow-install-hooks
```

The matching environment variables are `ZED_PKG_ALLOW_NATIVE_DEPS=1` and `ZED_PKG_ALLOW_INSTALL_HOOKS=1`. `--native-manager <name>` / `ZED_PKG_NATIVE_MANAGER` pins manager selection for reproducible CI and testing.

A package manifest names native packages, never an installer command. Zed maps supported manager identifiers to fixed argument vectors and passes every package spec as a separate argument. Unknown managers, option-shaped package specs, whitespace, control characters, and conflicting graph requirements fail closed.

## Lifecycle

For one install transaction Zed:

1. resolves the complete Zed dependency graph;
2. validates all lifecycle permissions before invoking a native package manager;
3. selects one native package manager supported by every package that declares native prerequisites;
4. de-duplicates and installs the graph's native packages once;
5. copies each source artifact from the immutable store into a writable staging directory;
6. runs `pre-install` hooks;
7. runs the package `[build]` command, when declared and allowed;
8. runs `post-install` hooks;
9. promotes the finalized staged artifact into the platform build cache;
10. materializes the cached artifact into the consumer project.

Hooks never use the immutable global store or the consumer project as their working directory. A failed native install, hook, build, or declared-output check prevents materialization and leaves the project transaction uncommitted. Native package managers change host state outside the project transaction, so Zed completes all permission and compatibility preflight before invoking one.

## Hook environment

Lifecycle commands receive stable package context:

- `ZED_INSTALL_PHASE`, `ZED_INSTALL_PACKAGE`, and `ZED_INSTALL_VERSION`;
- `ZED_INSTALL_PLATFORM`, `ZED_INSTALL_ROOT`, and `ZED_INSTALL_SOURCE`;
- `ZED_INSTALL_TARGET` when a polyglot target is selected;
- `ZED_NATIVE_MANAGER` and JSON `ZED_NATIVE_PACKAGES`;
- `ZED_INSTALL_MODULES` / `ZED_BUILD_MODULES` when build dependencies exist.

The compatibility variables `ZED_BUILD_PLATFORM`, `ZED_BUILD_SRC`, and `ZED_BUILD_TARGET` remain available to existing build commands.

## Nix

Outside a Nix build, the `nix` manager installs into a content-addressed profile below `$ZED_PKG_HOME/native/nix/v1/`. Zed reuses that profile for the same ordered package set and injects its `bin`, `pkg-config`, CMake, include, and library paths into lifecycle commands. It never modifies the user's default Nix profile.

Inside a Nix build sandbox, Zed does not invoke a package manager. Native prerequisites must be supplied by the derivation through `nativeBuildInputs` or `buildInputs`, and the manifest must offer a `nix` route. Once the derivation has provided those inputs, set `ZED_PKG_NATIVE_DEPS_PROVIDED=1`; Zed validates the graph and proceeds without running `nix profile install`.

For cross compilation, package-manager selection is not a substitute for target libraries. Build-machine tools belong in `nativeBuildInputs`; libraries linked into the target artifact belong in the cross `buildInputs` supplied by `pkgsCross` or an equivalent cross derivation.
