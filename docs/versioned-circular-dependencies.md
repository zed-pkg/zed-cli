# Exact-version circular dependencies

This document defines how Zed represents and materializes dependency graphs that
contain both circular edges and more than one exact version of the same package.
It complements the ordinary constraint solver, which intentionally selects one
version per `org/name` when all requirements can share a version.

The implementation lives in `src/versioned_graph.rs` and is exposed through
`zed graph materialize`.

## Canonical example

```text
A@1 -> B@1
B@1 -> A@2
A@2 -> B@0
B@0 -> A@2
```

The resolved graph contains exactly four nodes and four edges. The final edge is
not expanded into another copy of `A@2`. It closes the cycle by pointing to the
existing exact `A@2` node.

`A@1` and `A@2` are different nodes. Seeing the package name `A` twice is not a
cycle by itself.

## Exact node identity

A materialized node is identified by all of the following:

1. registry identity;
2. organization and package name;
3. exact version;
4. immutable artifact digest.

The public dependency-graph contract stores the first three fields in
`PackageVersionIdentity` and stores the digest on `ResolvedDependencyNode`.
Zed joins them before cycle reporting or filesystem materialization.

A graph that omits an artifact digest is rejected. Digests must use canonical
`sha256:<64 lowercase hexadecimal characters>` spelling and cannot be all zero.

## What is authoritative

The exact resolved graph is authoritative. Directory shape is not.

A production materialization accepts a byte-canonical graph whose
`graph_digest` verifies. Roots, nodes, edges, artifact digests, target, enabled
features, registry checkpoints, and lock digest therefore remain explicit and
replayable. A filtered graph projection is rejected because it is not complete
lock authority.

The local fixture-plan entry point may finalize a graph with no digest. It is
for tests and development fixtures; normal users should supply a verified graph
and let Zed bind its nodes to the content-addressed store.

## Detection algorithm

Zed builds a deterministic adjacency map keyed by exact package-version
identity. It then performs an iterative depth-first traversal.

Each node has one of three states:

- unseen;
- visiting, with its index in the active path;
- complete.

An edge to an unseen node descends into that node. An edge to a complete node is
ordinary graph sharing, such as the bottom point of a diamond, and is not a
cycle. An edge to a visiting exact node is a cycle-closing back edge.

For a back edge, Zed:

1. records the closed path from the earlier active node through the current
   node and back to the target;
2. rotates the path into a canonical deterministic spelling;
3. adds the edge to the materialized graph;
4. does not recursively expand that edge again.

The traversal uses an explicit frame stack rather than Rust call-stack
recursion. A deep hostile graph therefore cannot cause a recursive stack
overflow. Graph materialization is bounded to 50,000 nodes and 500,000 edges.
Cycle diagnostics are bounded to 4,096 deterministic witnesses so a hostile
strongly connected graph cannot flood the terminal or telemetry pipeline.

The detector reports deterministic DFS cycle witnesses rather than attempting
to enumerate every mathematically distinct simple cycle. Enumerating every
simple cycle can be exponential. Every cyclic strongly connected region still
produces a witness.

## Finite symlink layout

Package payloads remain extracted once in the global content-addressed store.
A project receives a finite overlay:

```text
project/
  zed_modules/
    <root-org>/<root-name> -> exact root node
  .zed/versioned-graph/v1/graphs/
    <graph-digest>/
      <source-binding-digest>/
        graph.json
        cycles.json
        .complete.json
        nodes/
          <stable-exact-node-key>/
            node.json
            root/
              <payload entries> -> immutable store payload entries
              zed_modules/
                <dep-org>/<dep-name> -> exact target node root
```

There is one overlay root per exact node and one dependency link per exact edge.
The `B@0 -> A@2` back edge is therefore one symlink to the already-created
`A@2` overlay root. It does not create `A@2/B@0/A@2/...` copies.

The stable node key includes a readable `org+name@version` prefix plus a hash of
the complete exact identity. Human-readable names remain useful while registry
identity and artifact digest still prevent collisions.

### Graph digest versus source-binding digest

`graph_digest` identifies semantic lock authority. It does not include local
filesystem paths.

The source-binding digest identifies one physical projection of that graph. It
hashes the graph digest plus the canonical local source path and artifact digest
for every node. Moving the store creates another finite generation without
changing graph identity. Repeating a materialization with the same graph and
sources reuses and verifies the existing generation.

## Atomicity and validation

Zed stages a complete generation next to its final location, writes canonical
graph and cycle records, writes a completion marker, and atomically renames the
staging directory. Concurrent processes may build the same generation; the
winner publishes it and the loser verifies and reuses it.

Before an existing generation is reused, Zed verifies:

- graph and source-binding digests;
- exact canonical graph bytes;
- deterministic cycle records;
- one real node directory per exact node;
- exact node metadata;
- every payload entry is a symlink to the expected immutable source;
- every dependency edge is a symlink to the expected exact target.

The project-level `zed_modules` root is replaced through a sibling transaction
backup. A failed publication restores the previous root.

Payload sources and the project must be disjoint. A source containing a
`zed_modules` entry is rejected so a previously materialized dependency tree
cannot be mistaken for immutable package payload.

## Copy mode and Windows

A circular graph cannot be naively copied recursively: that recreates the same
subtree forever. A parallel-version graph also cannot be flattened without
losing parent-specific selection.

The exact-graph command therefore fails closed in copy mode. It never silently
falls back to recursive copying. The ordinary flat installer remains available
for acyclic graphs that select one version per package coordinate.

On Unix, exact graphs use directory symlinks. On platforms where directory
symlinks are unavailable, exact-graph symlink materialization fails with an
explicit diagnostic. A future Windows implementation must use a reviewed finite
junction/symlink overlay with the same graph and safety invariants; copy fallback
is not acceptable for cycles.

## Terminal and ORE logging

Every deterministic cycle witness emits one warning through the Rust SDK from
`github.com/ores-otel/ores.otel.log`. Console output is enabled at the CLI
boundary, so the user sees the exact path and closing edge.

Structured fields include:

```text
event = zed.install.circular_dependency
cycle_id = sha256:<digest of the exact closed path>
path = [exact node labels including version and artifact digest]
closing_from = <exact node label>
closing_to = <exact node label>
target_node_key = <finite overlay node key>
target_artifact_digest = sha256:<artifact digest>
resolution = exact-node-symlink
graph_digest = sha256:<semantic graph digest>
materialization_digest = sha256:<local source-binding digest>
```

A representative terminal message is:

```text
circular dependency detected: registry::org/a@2#... -> registry::org/b@0#... -> registry::org/a@2#...; closing edge ... reuses the existing exact node through a symlink
```

The warning is evidence of deliberate cycle handling, not a failure by itself.
An incompatible or malformed graph still fails validation.

## CLI

Materialize a canonical graph from the configured Zed store:

```sh
zed --home "$ZED_PKG_HOME" graph materialize \
  --graph exact-resolution.json \
  --project . \
  --mode symlink
```

Every artifact digest named by the graph must already exist in the store. Zed
reports the exact missing node and store path instead of fetching or guessing.

Use an explicit local source plan for an isolated fixture:

```sh
zed graph materialize \
  --plan tests/fixtures/versioned-cycle/plan.json \
  --project "$TMPDIR/cycle-project"
```

The command prints ORE warnings as cycles are recognized and prints one JSON
materialization report as its final stdout line.

## Verification

Product gates:

```sh
cargo fmt --all --check
cargo test --locked --lib versioned_graph
cargo test --locked --test versioned_dependency_cycles -- --nocapture
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

The integration test exercises the real `zed` binary, the four-node example,
visible ORE output, finite node and edge counts, payload symlinks, exact back-edge
reuse, idempotent replay, content-addressed store binding, and copy-mode refusal.

`zed-pkg-test/zed-pkg-e2e` independently reconstructs the same graph outside the
product crate and runs the public executable in normal and replay modes.

## Current boundary and next integration step

This implementation provides the exact-version graph validation,
materialization, diagnostics, and public CLI substrate. The ordinary
manifest-driven solver and `.zpkg.lock` v1 still intentionally carry one selected
version per `org/name`.

Automatic fallback from an unsatisfiable one-version solve to an isolated
multi-version solve requires a separately reviewed lockfile extension that
persists exact roots and edges. That extension must reuse this materializer; it
must not invent a second directory-derived graph or weaken frozen replay.
