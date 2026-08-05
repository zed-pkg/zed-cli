# CLI package dependency graph

`zed-cli` is both a native Rust crate and a Zed package. Those manifests serve
different package managers, but they preserve the same architectural layers
without depending on accidental sibling checkouts.

## Required Zed edges

The root `.zpkg.toml` imports the canonical repository packages:

- `zed-pkg/zed-interfaces`, the shared manifest, protocol, resolution, and
  package-model contract;
- `zed-pkg/zed-clients`, the polyglot SDK package, which exposes its Rust SDK as
  one target while retaining repository-level identity and its own dependency
  on `zed-interfaces`;
- `zed-pkg/zed-lock`, the concrete kernel-backed, event-driven locking package.

The CLI must not depend on target-like coordinates such as
`zed-clients-rust`; Zed package identity remains `zed-clients`, and consumers
select targets through the package manifest. It must also not invent an umbrella
`zed-lib` or `zed-libs` coordinate. Reusable layers become dependencies only
when a real repository, source boundary, package manifest, and test contract
exist.

The CLI never imports `zed-infra`. Infrastructure remains independently owned
operational composition.

## Native Rust edges

`Cargo.toml` pins immutable commits for the Rust contracts it compiles:

- `zed-interfaces` remains pinned to its reviewed interface commit;
- `zed-lock` is pinned to hardened v0.1.1 merge commit
  `a0dc78d385bc3ab553d3027b427f5f1428239c9c`.

The original standalone lock extraction was published as v0.1.0 at commit
`0fc100afc3cd60b5ce091b4207f910bf08f2cfb7`. That release is immutable and is
not retargeted. Version 0.1.1 adds the actual Rust 1.88 MSRV, corrected Zed
package metadata, fail-closed package/provenance contracts, three-platform
conformance, and reviewed crate/checksum artifacts without changing the locking
API.

The in-tree `crates/zed-lock` copy was removed by the standalone dependency
migration and must not return. `Cargo.lock` must contain exactly one `zed-lock`
entry at version 0.1.1 and the exact hardened commit.

The clients relationship remains represented in the Zed graph, so Zed can
resolve and verify the reusable polyglot package without coupling ordinary Cargo
builds to a sibling `zed-clients` path.

## Lock authority

An empty placeholder `.zpkg.lock` is not valid evidence. The CLI lock remains
absent until a real resolver run can record immutable versions, artifact
digests, sizes, formats, and provenance; it must never be hand-authored merely
to make a repository appear resolved.

The standalone `zed-lock` package has no Zed dependencies and likewise omits an
empty `.zpkg.lock`.

## CI

`.github/workflows/zed-package-graph.yml` checks out the CLI, interfaces,
clients, and exact hardened lock repositories and runs the graph validator. The
gate checks:

- canonical package names, versions, URLs, target ownership, and native routes;
- the exact Cargo `zed-lock` repository revision and `Cargo.lock` source;
- the native interfaces edge;
- the clients package's own interfaces edge and Rust target;
- absence of the removed internal lock crate;
- `.vendor/.zed` materialization and publication exclusions;
- infrastructure separation;
- rejection of invented umbrella library coordinates;
- rejection of stale placeholder locks;
- standalone lock provenance, changelog, security, and package-contract files.

The dependency-pin PR regenerates `Cargo.lock` with Cargo and verifies that no
non-`zed-lock` package entry changes before the ordinary full CLI matrix runs.
