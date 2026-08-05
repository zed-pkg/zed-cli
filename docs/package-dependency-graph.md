# CLI package dependency graph

`zed-cli` is both a native Rust crate and a Zed package. Those manifests serve
different package managers, but they must preserve the same architectural
layers without making standalone Cargo builds depend on sibling checkouts.

## Required Zed edges

The root `.zpkg.toml` imports the canonical repository packages:

- `zed-pkg/zed-interfaces`, the shared manifest, protocol, resolution, and
  package-model contract;
- `zed-pkg/zed-clients`, the polyglot SDK package, which exposes its Rust SDK as
  one target while retaining repository-level identity and its own dependency
  on `zed-interfaces`.

The CLI must not depend on the target-like coordinate `zed-clients-rust`; Zed
package identity remains `zed-clients`, and consumers select targets through
the package manifest. There is no canonical `zed-lib` or `zed-libs` repository
today. If that reusable layer is introduced, its real package becomes a
required edge here and in the fleet audit. Until then, validators reject an
invented coordinate.

The CLI never imports `zed-infra`. Infrastructure remains independently owned
operational composition.

## Native Rust edge

`Cargo.toml` retains the direct immutable `zed-interfaces` dependency used by
the implementation. The clients relationship is represented in the Zed graph,
so Zed can resolve and verify the reusable polyglot package without coupling
ordinary Cargo builds to a sibling `zed-clients` path.

## Lock authority

An empty placeholder `.zpkg.lock` is not valid evidence. The lock is absent
until a real resolver run can record immutable versions, artifact digests,
sizes, formats, and provenance; it must never be hand-authored merely to make a
repository appear resolved.

## CI

`.github/workflows/zed-package-graph.yml` checks out the CLI, interfaces, and
clients repositories as siblings and runs the graph validator. The gate checks
canonical package names and URLs, the native interfaces edge, the clients
package's own interfaces edge and Rust target, `.vendor/.zed` materialization,
publication exclusions, infrastructure separation, and stale placeholder locks.
