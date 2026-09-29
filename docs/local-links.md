# Local package links

Zed supports an explicit live-working-tree workflow for developing one package against another package without publishing an intermediate release.

The model intentionally mirrors the useful two-stage behavior of `npm link` while keeping mutable developer state outside Zed's reproducible package graph.

## Register a package working tree

From the package you are editing:

```sh
zed link .
```

`zed link` with no operand is equivalent.

Zed reads the package identity from `.zpkg.toml` and registers the canonical source directory under:

```text
$ZED_PKG_HOME/links/<org>/<name>.json
```

The directory name is not package authority. `[package].org` and `[package].name` are.

Registration does not publish, pack, copy, resolve, or modify a lockfile.

## Consume the registered package

From another project:

```sh
zed link org/name
```

For npm-style scoped spelling, this is also accepted:

```sh
zed link @org/name
```

A bare package name is accepted only when it identifies exactly one registered package:

```sh
zed link name
```

Ambiguous bare names fail and require the full `org/name` identity.

The consumer receives a live directory symlink, so edits in the source working tree are immediately visible through the consumer path.

## One-command path shortcut

This form combines registration and consumption, similar to linking a local directory with npm:

```sh
zed link ../some-package
```

The referenced package is registered from its authored `.zpkg.toml` identity and then consumed in the current project.

`zed link .` is special: it registers the current package and does not create a self-link.

## Consumer projections

Every local link is projected into Zed's universal package directory:

```text
zed_modules/<org>/<name>
```

The default `--adapter=auto` also creates the Node projection when the consumer contains `package.json`:

```text
node_modules/@<org>/<name>
```

You can select the projection explicitly:

```sh
zed link org/name --adapter=none
zed link org/name --adapter=node
```

The source working tree remains the one authoritative live target. Adapter paths are projections of the same source, not copies.

## Reproducibility boundary

Local links are explicit development state. They are deliberately not part of ordinary package resolution.

`zed link` does not mutate:

- `.zpkg.toml`;
- `.zpkg.lock`;
- immutable package-store artifacts.

An ordinary `zed install`, including `zed install --frozen`, never discovers the machine-local link registry merely because a registration exists. This prevents a checkout from succeeding only on one developer's machine while claiming lockfile reproducibility.

Use authored `[overrides.path]` when a project intentionally wants source-controlled local path override metadata. Use `zed link` for machine-local iterative development state.

## Ownership and safe unlinking

Zed writes a consumer ownership receipt under:

```text
.zed/local-links/<org>/<name>.json
```

When the destination already exists, Zed preserves it before installing the live link:

- an existing symlink target is recorded;
- an existing package directory is moved to a reversible backup beneath `.zed/local-link-backups/`;
- a non-directory, non-symlink destination is rejected.

Remove a consumer link with:

```sh
zed unlink org/name
```

Zed verifies that every managed destination is still a symlink to the source it originally installed. If another tool or developer changed that destination, unlink fails closed instead of deleting unknown state.

After verification, the previous symlink or directory is restored.

## Unregister a working tree

From the registered package itself:

```sh
zed unlink
```

or:

```sh
zed unlink .
```

From another directory, explicitly remove a machine-wide registration:

```sh
zed unlink org/name --global
```

Unregistering does not walk other projects and remove their existing links. Their ownership receipts remain explicit local state, and a later consumer unlink still validates the source and destination before making changes.

## Inspect registrations

```sh
zed links
```

Machine-readable form:

```sh
zed links --json
```

The listing revalidates each registration. Missing sources, invalid manifests, and package-identity drift are reported as invalid rather than silently accepted.

## Safety properties

The local-link lifecycle applies the same path-boundary philosophy as package materialization and authored path overrides:

- source and consumer paths are canonicalized;
- the source must be a directory with a regular `.zpkg.toml`;
- package identity must still match the registered `org/name`;
- sources that overlap Zed's package-install or transaction-staging directories are rejected;
- destination ancestors are canonicalized before mutation so a symlinked parent cannot redirect a package link outside the consumer project;
- source/destination overlap and self-linking are rejected;
- registration and receipt state files must be regular files, not attacker-supplied symlinks;
- machine-wide registry mutations use a Zed lock;
- consumer mutations use the canonical project lock;
- state writes use staged files and replacement rather than partial in-place serialization;
- on Windows, Zed requests a real directory symlink and reports the privilege/Developer Mode problem if the platform refuses it. Zed does not silently fall back to a copy because a copy would no longer provide live-link semantics.

## Difference from npm's global executable linking

npm's global registration step can also expose package executables through its global bin directory. Zed intentionally keeps that ownership surface separate for now.

`zed global install` owns Zed's managed global executable copies and its rollback/state model. `zed link` does not overwrite or bypass that state simply to mimic npm's bin symlink side effect.

If source-linked executables are added later, they should integrate with the existing managed-global-bin ownership model so unlink, collision handling, checksums, and restoration remain deterministic. Dependency/source linking does not need to compromise that boundary.
