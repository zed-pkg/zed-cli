# Complete dependency graph solving

`zed install` resolves the complete active requirement set before any project
lockfile, adapter file, staging directory, or materialized dependency is
changed. This closes the false-conflict case where the first path selects a
newer version even though a lower published version satisfies every path.

## Deterministic model

For each `org/name`, the solver retains every requirement together with its
root-to-package provenance path. Candidate versions are considered in
canonical descending order. A candidate contributes the dependencies declared
by that exact immutable package artifact. When a downstream conflict is found,
the complete branch state is discarded and the next candidate is explored.
Constraints contributed only by a rejected candidate therefore cannot leak
into another branch.

New provenance paths are propagated through dependencies that are already
selected. This is required for diamonds and cycles: discovering another path
to a selected package may add a requirement path to each of its children even
when no selected version changes.

An unsatisfiable graph reports the active requirements and their deterministic
paths. Candidate failure nesting is bounded in the rendered diagnostic, while
the solve itself remains complete.

## Artifact and mutation boundary

Graph correctness does not introduce another downloader. Package metadata and
candidate manifests use the existing bounded `FetchPool`; immutable artifacts
remain serialized across processes by their per-SHA blocking operating-system
locks. Worker completion order cannot choose the graph because the solver
consumes results in deterministic sequence order.

Once solved, exact registry selections are exposed only to the root consumer
manifest through a scoped, panic-safe context. Package manifests loaded from
the immutable store are unchanged. The established transactional installer
continues to own lockfile generation, adapters, staging, rollback, cache use,
and final materialization.

`zed install --frozen` is different by design: the committed lock graph is the
authority. Frozen replay verifies and acquires that exact graph without
re-solving or rewriting it.

## Required regression properties

The permanent test surface must retain all of these properties:

1. overlapping ranges select the highest version satisfying every path;
2. direct dependency declaration order does not affect the graph;
3. worker/network completion order does not affect the graph or diagnostic;
4. rejected candidate dependencies do not remain active;
5. backtracking may cross more than one package coordinate;
6. every incompatible requirement path appears deterministically;
7. diamonds and cycles terminate with one selected version per coordinate;
8. frozen replay consumes the exact lock graph without solving;
9. candidate acquisition retains the configured worker bound and per-SHA lock;
10. normal install and prefetch consume the same prepared graph.

Linear: DEN-1553. Related foundations: DEN-1505 and DEN-1522.
