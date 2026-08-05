# CLI package dependency graph

`zed-cli` is both a native Rust crate and a Zed package. Those manifests serve
different package managers, but they must preserve the same architectural
layers without making standalone Cargo builds depend on an accidental sibling
checkout.

## Required Zed edges

The root `.zpkg.toml` imports the canonical repository packages:

- `zed-pkg/zed-interfaces`, the shared manifest, protocol, resolution, and
  package-model contract;
- `zed-pkg/zed-clients`, the polyglot SDK package, which exposes its Rust SDK as
  one target while retaining repository-level identity and its own dependency
  on `zed-interfaces`;
- `zed-pkg/zed-lock`, the concrete kernel-backed, event-driven lock library
  extracted from `crates/zed-lock`.

The CLI must not depend on target-like coordinates such as
`zed-clients-rust`; Zed package identity remains `zed-clients`, and consumers
select targets through the package manifest. It must also not invent an umbrella
`zed-lib` or `zed-libs` coordinate. Reusable layers become dependencies only
when a real repository, source boundary, package manifest, and test contract
exist. `zed-lock` satisfies that rule; a hypothetical catch-all library does
not.

The CLI never imports `zed-infra`. Infrastructure remains independently owned
operational composition.

## Native Rust edges

`Cargo.toml` retains the direct immutable `zed-interfaces` dependency used by
the implementation. The clients relationship is represented in the Zed graph,
so Zed can resolve and verify the reusable polyglot package without coupling
ordinary Cargo builds to a sibling `zed-clients` path.

The lock implementation is being migrated in two explicit stages:

1. **Package-graph adoption.** The Zed graph imports `zed-pkg/zed-lock`, CI
   checks out the standalone repository, validates its package metadata, and
   requires byte-for-byte parity for `Cargo.toml`, `LICENSE`, `SECURITY.md`,
   `src/`, `tests/`, and `examples/`. During this stage Cargo deliberately
   retains `zed-lock = { path = "crates/zed-lock" }`, so no source authority is
   changed before the external repository is hardened and reviewed.
2. **Cargo authority switch.** A later PR pins Cargo to one immutable commit in
   `zed-pkg/zed-lock`, regenerates `Cargo.lock`, reruns all cross-process and
   platform tests, and only then removes the internal copy. Source removal and
   dependency-source changes are not hidden in the package-manifest PR.

This ordering avoids both a dual-implementation fork and an unreviewed package
coordinate.

## Lock authority

An empty placeholder `.zpkg.lock` is not valid evidence. The lock is absent
until a real resolver run can record immutable versions, artifact digests,
sizes, formats, and provenance; it must never be hand-authored merely to make a
repository appear resolved.

The same rule applies to `zed-lock`: because it currently has no Zed package
dependencies, its standalone repository omits `.zpkg.lock` rather than
committing `version = 1` as a twelve-byte placeholder.

## CI

`.github/workflows/zed-package-graph.yml` checks out the CLI, interfaces,
clients, and lock repositories as siblings and runs the graph validator. The
gate checks:

- canonical package names, versions, repositories, and native registry routes;
- the native interfaces edge and transitional internal lock edge;
- the clients package's own interfaces edge and Rust target;
- complete internal/standalone lock-source parity;
- `.vendor/.zed` materialization and publication exclusions;
- infrastructure separation;
- rejection of invented `zed-lib` coordinates;
- rejection of stale placeholder locks.

The package graph remains red until the standalone `zed-lock` hardening PR is
merged to its canonical default branch. That is an intentional merge-order
gate, not a reason to weaken validation.
