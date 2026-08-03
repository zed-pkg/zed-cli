# mise interoperability

`zed-pkg` is an independent multi-language package manager. It is unrelated to the Zed editor. In this document, `zed` is the `zed-pkg` command-line executable.

## Responsibility boundary

The integration keeps each system authoritative for a different layer:

| Layer | Authority |
| --- | --- |
| Developer runtimes and CLI tools | mise configuration and lockfiles |
| Zed packages and artifacts | `.zpkg.toml`, `.zpkg.lock`, the Zed store, and `zed_modules/` |
| Native application dependencies | npm/pnpm, Cargo, Maven/Gradle, uv/pip, Go modules, pub, and their native lockfiles |
| Cross-layer command execution | `zed develop` / `zed dev` and project tasks |

The mise adapter never translates Zed dependencies into mise tools and never lets mise mutate the Zed dependency graph.

## Recommended companion workflow

A full mise-managed repository may continue to use all ordinary mise features:

```toml
# mise.toml
[tools]
node = "22"
python = "3.12"

[tasks.zed-install]
run = "zed install --frozen"
```

```sh
mise lock
mise install
mise exec -- zed install --frozen
mise exec -- zed dev -c 'npm test'
```

This workflow does not require `zed env import`. mise remains responsible for evaluating its complete configuration, environment, tasks, and trust policy.

## Read-only plan import and verification

The first native interoperability slice exposes:

```sh
zed env import mise
zed env verify mise --frozen
```

Use explicit files when repository conventions would otherwise be ambiguous:

```sh
zed env verify mise \
  --config mise.toml \
  --lock mise.lock \
  --frozen \
  --json
```

The commands parse manager files directly. They do **not**:

- invoke a `mise` executable;
- search parent directories;
- load global or user-local mise configuration;
- execute templates, hooks, tasks, plugins, or install scripts;
- install tools;
- modify mise files, Zed manifests, Zed locks, or the Zed store.

This makes plan inspection usable in credential-free pull-request checks and on machines where mise is not installed.

## Supported import surface

The initial adapter supports one project-local input selected from:

- `mise.toml`;
- `.mise.toml`;
- `.tool-versions`.

Supported `mise.toml` data is intentionally narrow:

- `[tools]` entries containing one version string;
- tool tables containing `version` and optional `os`;
- `[settings]` entries for `lockfile`, `locked`, and `lockfile_platforms`, subject to the safety rules below.

Supported `mise.lock` data includes:

- one exact identity per configured tool;
- exact `version` and optional `backend`;
- platform-specific URL, size, and SHA-256, SHA-512, or BLAKE3 checksum metadata.

The normalized plan retains the authored requirement, exact locked version, provider/backend, platform set, checksums, config path, lock path, and a deterministic digest of the normalized manager-native state.

## Frozen verification

`--frozen` changes import into a fail-closed reproducibility check. It requires:

- a project-local lockfile;
- exact one-to-one tool coverage between config and lock;
- an exact non-moving resolved version for every tool;
- at least one platform artifact identity per locked tool;
- a cryptographic checksum for every represented platform artifact;
- valid checksum algorithms and digest lengths;
- safe project-relative config and lock paths;
- no unsupported manager semantics that would be lost during normalization.

A URL and byte size are retained as provenance and participate in drift detection, but a URL alone is not accepted as immutable artifact identity. The initial frozen adapter intentionally rejects a lock entry whose backend did not record a checksum rather than weakening verification or pretending that an exact version string proves the downloaded bytes.

`settings.lockfile = false` is incompatible with frozen verification.

`settings.locked = true` is rejected by the project-local adapter because mise defines that setting with global scope: verifying it faithfully would require importing user-global tools, which would violate the Zed isolation contract. Use `zed env verify mise --frozen` for project-local verification and keep any global mise policy outside the imported plan.

## Deliberately unsupported in this slice

The importer rejects unsupported state rather than silently dropping it. Current examples include:

- `[env]`, dotenv loading, path composition, templates, and secrets;
- tasks, hooks, aliases, includes, and task files;
- custom plugin declarations and install/post-install commands;
- multiple configured versions for one tool;
- tool options that affect artifact identity;
- backend-specific transitive lock sections;
- simultaneous candidate configs without an explicit `--config` choice.

Environment/trust support and native task-DAG support are separate implementation tracks. Until those contracts land, use ordinary `mise exec` or `mise run` for complete mise semantics.

## Deterministic output

Human verification output reports the selected config, lock, tool count, and plan SHA-256. Machine-readable verification output has a stable envelope:

```json
{
  "manager": "mise",
  "config": "mise.toml",
  "lock": "mise.lock",
  "tools": 2,
  "environment_plan_sha256": "…",
  "verified": true
}
```

`zed env import mise --json` emits the normalized shared `EnvironmentPlan` itself.

The manager-source digest is semantic rather than presentation-based: changing TOML whitespace or key order does not change it, while changing a resolved version, backend, platform artifact, checksum, URL, byte size, or supported setting does.

## Lockfile naming

When no `--lock` is supplied, the adapter follows mise's adjacent-file convention by replacing the config file's `.toml` extension with `.lock`:

```text
mise.toml        → mise.lock
.mise.toml       → .mise.lock
mise.test.toml   → mise.test.lock
```

`.tool-versions` has no implicit lock path. Authoring import works without a lock; frozen verification must receive an explicit compatible lock with `--lock`.

## Security invariants

- Explicit paths are canonicalized and must remain inside the project root, including through symlinks.
- Parent and global configuration never participate implicitly.
- Import and verification are read-only.
- Unknown fields fail with their exact TOML path.
- Lock/config drift reports missing and extra tool names.
- Frozen artifact verification requires cryptographic identity rather than trusting a version label or mutable URL.
- No arbitrary shell command is accepted as an activation policy; the shared plan exposes only the fixed `zed install --frozen` activation boundary.

## Planned follow-up

The remaining mise-parity work includes deterministic export and three-way merge planning, typed environment values and trust, task-DAG execution, native tool backends, activation in `zed dev`, offline replay, tamper tests, and cross-platform certification in `zed-pkg-test` and `zed-e2e`.
