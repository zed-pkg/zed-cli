# Static inspection and editor interoperability

`zed inspect --root ABSOLUTE_PATH --format json` is the canonical,
non-mutating analysis boundary for Zed package tooling. Editor integrations
should consume this protocol instead of reimplementing manifest, lock, Git,
mise, or Nix rules.

## Invocation

```sh
zed inspect --root /absolute/project --format json
```

The project root is explicit, existing, and absolute. JSON is currently the
only format and remains the default when `--format` is omitted. Inspection is
always offline: the command is recognized before ordinary CLI configuration,
credential loading, transaction recovery, store initialization, or registry
selection.

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
- contact a registry or any other network service;
- load credentials or user-global tool configuration.

Every recommended action is represented as `argv`, never a shell string, and
declares whether it mutates the project, requires network access, or executes
package code. Plugins must show those properties before offering a one-click
action and should execute only actions they explicitly allowlist.

## Shared consumer model

The protocol is versioned with `schema_version`. Consumers must reject an
unsupported major schema version and tolerate additive fields within a major
version. `warning` and `error` are the emitted severity tokens; consumers may
also accept `info` for forward-compatible v1 reports.

Suggested integration per host:

| Host | Local analysis |
| --- | --- |
| VS Code / TypeScript | Spawn `zed inspect --root ABSOLUTE_PATH --format json` |
| Eclipse / Java | Spawn the same process protocol and decode the v1 JSON schema |
| Xcode / Swift | Spawn the same process protocol and decode the v1 JSON schema |
| Sublime Text / Python | Spawn the same process protocol and decode the v1 JSON schema |
| Native or Rust host | Spawn the same process protocol unless an independently versioned binding is provided |

The process boundary is preferred for editor plugins because one installed CLI
provides the same analyzer everywhere. A plugin may fall back to a local
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

The schema fixture lives at `schemas/inspect-v1.schema.json`. The `interop`
object reports separate `detected`, `declared`, and `verified` states for Git
submodules, mise, and Nix so consumers do not mistake discovery for reproducible
readiness.
