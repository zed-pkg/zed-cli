# Circular dependency resolution

## Identity and termination

The recursive resolver treats the active provenance path as version-qualified
coordinates: `org/name@version`. When a dependency points back to a coordinate
already on that active path, the edge is a **back-edge**. Zed records the
constraint but does not propagate dependencies through that edge a second time.
This is what prevents recursive mirror trees and stack growth.

For a compatible cycle such as:

```text
A@1.0.0 -> B@1.0.0 -> A@1.0.0
```

resolution selects each coordinate once, downloads each content hash at most
once, and later materializes the selected package from the canonical
content-addressed store. Symlink mode points the project at that canonical
materialization; it does not copy `A/B/A/B/...` into nested directories.

Every recognized back-edge emits one deduplicated `WARN` record through the
Rust logger from `ores-otel/ores.otel.log`. The human stderr line includes:

- the exact version-qualified cycle path;
- the closing edge and requirement;
- `strategy=canonical-store-symlink`; and
- `recursive-copy=stopped`.

The structured record uses event name `dependency_cycle_detected` and fields
under `cycle.*`, allowing the same event to be routed to OpenTelemetry later
without changing the resolver contract. Stdout remains protocol-safe.

## Same package at different versions

The dependency-graph wire contract in `zed-interfaces` already identifies nodes
by registry, organization, name, and exact version, so it can represent:

```text
A@1 -> B@1 -> A@2 -> B@0 -> A@2
```

The current `.zpkg.lock` v1 and project materialization layout are still keyed
by `org/name`, however. They therefore cannot persist two selected versions of
the same coordinate without ambiguity. Until the lock/materialization migration
lands, such a graph fails deterministically with both version-qualified
provenance paths; it is never silently flattened to one version and never
expanded recursively on disk.

The migration must be contract-first:

1. persist exact graph nodes and edges using the existing
   `zpkg/dependency-graph/v1` identity;
2. address canonical project graph nodes by immutable artifact digest;
3. create dependency-local symlinks between graph nodes rather than copying
   package trees;
4. expose only direct roots at the consumer's `zed_modules/` boundary; and
5. make frozen replay restore the exact graph without re-solving.

## Regression matrix

The in-repository tests cover compatible two-node cycles, self-cycles with an
acyclic tail, 64-node rings, multiple roots entering one cycle, warm-store
idempotency, deterministic divergent-version rejection, bounded diagnostics,
and absence of lock/module mutation when resolution fails.
