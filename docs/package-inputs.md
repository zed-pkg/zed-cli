# Package inputs, Git submodules, and Zed packages

Zed packages and Git submodules solve different problems. A package manifest and lockfile define installable, versioned dependencies. A Git submodule preserves an independently versioned source checkout inside an editable workspace. Kubernetes deployment composition belongs to OCI images and GitOps, not to either mechanism.

## Decision rule

Use a **Zed package** for a reusable library, interface/schema bundle, generated SDK, CLI, build tool, or other dependency that consumers install without editing its source.

Use a **Git submodule** only when the checkout itself is part of the workflow:

- an editable polyrepo workspace;
- a commit-pinned inventory/composition repository;
- embedded source that must be built as part of the parent package; or
- a large reference/experiment that should retain independent history.

Do not use a submodule merely to make a library available at build time. Publish that library as a Zed package instead. Do not point Argo CD at a path inside a submodule; deploy immutable OCI digests from the owning application repository.

## Publication safety boundary

`zed pack` and `zed publish` verify every included submodule before creating an artifact. Included submodules must be initialized, pinned to the recorded gitlink, clean, and recursively settled. Excluded submodule trees remain optional.

The pack preflight also inspects the package Git work tree and initialized nested Git work trees, including submodules. A Git-ignored, untracked regular file that would otherwise enter an artifact fails closed. This prevents files such as local `.env` credentials, generated private keys, editor state, or machine-specific build output from silently becoming package bytes.

There are three explicit resolutions:

1. Exclude the path with `.zedignore` or `[publish].exclude`.
2. Track the exact release input, using `git add -f` when the repository ignore rules require it.
3. Admit a narrowly scoped generated input with a tracked and clean `.zedinclude`.

Example:

```text
# .gitignore
dist/

# .zedinclude
dist/cli.wasm
dist/checksums/*.sha256
```

`.zedinclude` rules are project-relative, case-insensitive Zed globs. Blank lines and `#` comments are allowed. Absolute paths, `..`, empty path segments, backslashes, and negated rules are rejected. The allowlist itself must be a regular file committed without staged or unstaged changes, and it is never copied into the package artifact.

A broad rule such as `**` defeats reviewability and should not be used. Prefer exact files or one bounded generated directory. The allowlist does not relax submodule commit, cleanliness, or recursive-initialization checks.

## Composition repositories

A composition repository should classify every gitlink as one of:

- `workspace`: editable source checkout for local development;
- `inventory`: commit-pinned component catalog, not a build input;
- `embedded-source`: source intentionally included in one package artifact;
- `reference`: experiment or documentation input with no production dependency; or
- `legacy`: retained read-only while consumers migrate.

Only `embedded-source` may contribute package bytes. `workspace` and `inventory` gitlinks must be excluded from publication and must never serve as an Argo CD render path. Coordinated releases should record component package versions and OCI digests in a release manifest rather than treating the composition repository commit as the deployable artifact.

## Migration from submodules to Zed

When a gitlink is currently used only to compile or import a reusable component:

1. give the component its own `.zpkg.toml` and immutable release version;
2. publish its interface or library through Zed;
3. replace the source-path import with a locked Zed dependency;
4. certify installation from a clean checkout without initialized submodules; and
5. remove the gitlink after all consumers use the package.

Keep a submodule when engineers routinely edit both repositories together. A future remote-workspace layer can materialize those independent repositories while preserving their histories; that is a workspace feature, not package dependency resolution.
