# Static inspection and editor interoperability

`zed inspect --json` is the canonical, non-mutating analysis boundary for Zed
package tooling. Editor integrations should consume this protocol instead of
reimplementing manifest, lock, Git, mise, Nix, or update rules.

## Invocation

```sh
zed inspect --workspace /absolute/project --json
zed inspect --workspace /absolute/project --json --network
```

The default invocation is offline. `--network` is an explicit capability: it
contacts the configured Zed registry and asks for the latest CLI and direct
dependency versions. Recommendations carry `major`, `minor`, or `patch`
classification. Registry failures become bounded diagnostics; credentials are
never included in the report.

The command exits nonzero only when it cannot produce a protocol result. A
workspace with findings still returns a JSON report and normally exits zero;
consumers must use `summary.health`, not the process code, to render status.

## Safety contract

Inspection does not:

- recover or remove an interrupted transaction;
- resolve, install, build, fetch, or publish packages;
- rewrite `.zpkg.toml` or `.zpkg.lock`;
- invoke Git synchronization, mise, or Nix;
- execute package code;
- contact a registry unless `--network` is present.

Every recommended action is represented as `argv`, never a shell string, and
declares whether it mutates the project, requires network access, executes
package code, or needs confirmation. Plugins must show those properties before
offering a one-click action and should execute only actions they explicitly
allowlist.

## Shared consumer model

The protocol is versioned with `schemaVersion`. Consumers must reject an
unsupported major schema version and tolerate additive fields within a major
version. `info`, `warning`, and `error` are the stable severity tokens.

Suggested integration per host:

| Host | Registry API | Local analysis |
| --- | --- | --- |
| VS Code / TypeScript | `zed-clients` TypeScript client | Spawn `zed inspect --json` |
| Eclipse / Java | `zed-clients` Java client | Spawn `zed inspect --json`; JNI to `zed-lib` is optional |
| Xcode / Swift | `zed-clients` Swift client | Spawn `zed inspect --json`; the `zed-lib` C ABI is optional |
| Sublime Text / Python | `zed-clients` Python client | Spawn `zed inspect --json` |
| Native C or C++ host | `zed-clients` C/C++ client | Link `zed-lib` C ABI or C++ wrapper |
| Rust host | `zed-clients` Rust crate | Import `zed-lib` directly |

The process boundary is preferred for editor plugins because one installed CLI
provides the same analyzer everywhere. Native `zed-lib` bindings are useful for
fully bundled or offline integrations. A plugin may fall back to a local
scanner only to report that the canonical analyzer is unavailable; fallback
findings must identify themselves and must not claim canonical compatibility.

## Interop rules

Git submodule consumption is explicit:

```toml
[interop.git]
consume_gitmodules = true
```

Static inspection cross-checks that declaration with committed, regular
`.gitmodules` metadata. Mise verification reuses Zed's project-local frozen
importer without reading global configuration. Nix verification treats
`flake.nix` and `flake.lock` as data and never runs `nix develop`.

## Dependency direction

The reusable implementation is intentionally acyclic:

```text
zed-interfaces <- zed-lib <- zed-clients
       ^             ^            ^
       +-------------+------------+--- zed-cli
```

`zed-interfaces` owns shared DTOs and schemas. `zed-lib` owns local read-only
analysis and C/C++ interoperability. `zed-clients` owns registry transport in
each supported language. `zed-cli` composes all three and provides the process
protocol used by plugins.
