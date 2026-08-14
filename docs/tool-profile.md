# Frozen offline tool profiles

`zed-tool` is the low-level native-tool replay surface for exact
`EnvironmentLock` files. It does not resolve versions, contact a registry,
execute a manager or plugin, run an install script, or modify a global PATH.
The canonical catalog-backed user route is
[`zed install --cli`](cli-tools.md); it resolves and downloads exact built-in
runtime artifacts before delegating to this hardened profile installer.

The first slice proves that manager-neutral locked artifacts can be verified,
extracted through Zed's existing content-addressed store, and activated in one
project-local profile without requiring mise or another environment manager.

## Commands

```console
zed-tool [--lock .zed/environment.lock.toml] [--json] verify [--portable] [--plan-digest SHA]
zed-tool [--lock .zed/environment.lock.toml] [--json] list --target TARGET
zed-tool [--lock .zed/environment.lock.toml] [--json] install --target TARGET --offline [--profile .zed/tools] [--install-mode symlink|copy] [--home PATH]
```

The dedicated flags contract is `.tool-cli-flags.toml`:

- `ZED_TOOL_LOCK`
- `ZED_TOOL_JSON`
- `ZED_TOOL_PORTABLE`
- `ZED_TOOL_PLAN_DIGEST`
- `ZED_TOOL_TARGET`
- `ZED_TOOL_OFFLINE`
- `ZED_TOOL_PROFILE`
- `ZED_TOOL_INSTALL_MODE`
- `ZED_PKG_HOME`

## Frozen replay model

Installation performs these steps:

1. Canonicalize the current project and read one project-relative lock without
   following a symlink outside the checkout.
2. Parse TOML or JSON through `zed_interfaces::EnvironmentLock` and require
   portable validation.
3. Select exactly one locked variant per logical tool for the requested target.
   Multiple active versions are rejected until precedence is explicitly
   modeled and certified.
4. Reject every artifact format except `tar_gz` and `zip` in this first slice.
5. Find the authenticated bytes at
   `$ZED_PKG_HOME/cache/<sha256>.tar.gz`; the extension is the historical cache
   convention and the extractor still identifies gzip or ZIP by magic bytes.
6. Require the cached file's exact locked size and SHA-256. Zed package
   artifacts pass through the existing per-digest locked, traversal-safe,
   atomic package-store extraction. Catalog runtime archives marked with the
   authenticated `zed-pkg.archive-layout = "raw"` extension pass through the
   bounded raw-runtime extractor into a separate digest-keyed tool store.
7. Resolve every locked executable below the authenticated install root,
   rejecting missing files, symlink traversal, non-regular files, and missing
   Unix execute bits.
8. Reject duplicate executable or alias ownership. Collision checks are ASCII
   case-insensitive so a profile remains safe on Windows.
9. Stage a complete profile under `.zed/tools/v1/<target>`, fsync its state,
   and atomically replace the previous owned profile while holding the
   checkout-local operation lock.
10. In `symlink` mode, record the active profile's artifact digests as a
    distinct live store reference, so package installs and tool profiles do not
    overwrite each other's GC ownership. In `copy` mode, copy each complete
    install root under the profile and use project-relative command links, so
    no global-store reference is required at runtime.

On Unix, symlink-mode profile commands point at immutable store files. Copy-mode
profile commands point relatively at complete runtime roots inside the
project. On Windows, package-artifact commands are exact copied
executable/script files. A logical command without an extension inherits a
locked `.exe`, `.com`, `.cmd`, or `.bat` extension on Windows. Portable raw
runtime archives that contain Unix symlinks are currently GNU/Linux-only.

## Profile state

The deterministic `profile.json` records only:

- schema;
- normalized environment-lock SHA-256;
- target;
- install mode;
- logical tool name, original requirement, exact version, and backend;
- artifact SHA-256;
- relative install root; and
- exposed command names and relative locked source paths.

It contains no source URL, credential, environment value, timestamp, hostname,
process identifier, user-home path, or absolute store path. Replaying the same
lock and target against an intact profile reports `unchanged` and does not
rewrite profile state.

## Fail-closed boundaries

The staged installer rejects:

- online mode;
- missing cached bytes;
- wrong artifact size or SHA-256;
- unsupported archive formats;
- raw upstream runtime archives requested in symlink mode;
- raw archives that exceed the entry/size limits or contain traversal,
  absolute links, hard links, devices, or other unsupported entry types;
- missing target variants or multiple variants for one logical tool/target;
- executable and alias collisions;
- unsafe project, lock, profile, target, install-root, or executable paths;
- symlinked profile parents or authenticated artifact paths;
- non-regular or non-executable locked commands;
- lock/plan digest drift; and
- profile replacement or reference-recording failures.

A failed profile commit restores the prior active profile. Arbitrary backend
plugins, signatures, multiple active versions, update/outdated operations, lazy
shims, shell activation, and `zed dev` integration remain separate certified
slices under DEN-1437 and DEN-1442.
