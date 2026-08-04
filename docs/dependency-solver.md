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
when no selected version changes. A cycle-closing back edge remains an active
version constraint, but its provenance path is terminal and is not expanded
again; this preserves the safety check without producing an infinite sequence
of longer equivalent paths.

An unsatisfiable graph reports the active requirements and their deterministic
paths. Candidate failure nesting is bounded in the rendered diagnostic, while
the solve itself remains complete.

## Version-scheme and withdrawal policy

Requirements are matched according to the package's declared version scheme:

- semver and calver packages use their normalized range semantics;
- opaque versions are exact identifiers and never acquire semver meaning merely
  because an identifier contains digits or a requirement begins with `^`; and
- workspace members are checked with the same scheme-aware matcher as registry
  candidates.

Fresh resolution reads immutable version metadata before submitting an artifact
to the acquisition pool. A yanked candidate may be remembered as unavailable
for deterministic diagnostics, but its archive is not downloaded or extracted
into a fresh home. When every otherwise matching candidate is yanked, the error
points to lock-authoritative `zed install --frozen` replay. An existing exact
lock may continue to acquire and replay the withdrawn version; fresh solving may
not choose it.

## Artifact and mutation boundary

Graph correctness does not introduce another downloader. Package metadata and
candidate manifests use the existing bounded `FetchPool`; immutable artifacts
remain serialized across processes by their per-SHA blocking operating-system
locks. Worker completion order cannot choose the graph because the solver
consumes results in deterministic sequence order.

The solver batches one highest currently viable candidate for every active
unresolved coordinate before waiting. This lets a wide cold frontier saturate
the configured five-worker pool while keeping alternate versions lazy:
backtracking downloads another version only after the selected search branch
actually needs it. A warm run still reports zero downloads and reuses the
content-addressed store.

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
8. opaque package identifiers accept exact requirements only;
9. fresh solving rejects yanked candidates before archive acquisition;
10. frozen replay consumes the exact lock graph, including a previously locked
    version that was subsequently yanked;
11. a cold wide frontier reaches the configured five-worker bound;
12. a warm replay downloads zero artifacts;
13. candidate acquisition retains the per-SHA interprocess lock; and
14. normal install and prefetch consume the same prepared graph.

## Verification procedure

A candidate solver change is not complete after the focused overlap test alone.
The review head must pass all of the following on the same immutable commit:

```text
cargo fmt --all --check
cargo test --locked --lib install_graph::tests
cargo test --locked --lib install_graph::solver::tests
cargo test --locked --test e2e opaque_versions_require_exact_match -- --exact
cargo test --locked --test recursive_install_concurrency \
  recursive_http_prefetch_saturates_at_five_and_warm_runs_do_not_redownload -- --exact
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Repository CI must additionally retain frozen-lock integrity, Windows locking,
manifestless/polyglot installation, OCI copy-mode, Nix interoperability,
development-shell, formal review, agent-policy, and repository-hardening gates.
Independent black-box certification in `zed-pkg-test/zed-pkg-e2e#36` must build
one immutable product SHA and prove overlap, multi-coordinate backtracking,
rejected-constraint removal, deterministic unsatisfiable provenance, cycles,
yank-before-acquisition, and frozen replay through the public executable.

Temporary formatting, generation, or correction workflows are not product
evidence and must be absent from the final pull-request diff.

Linear: DEN-1553. Related foundations: DEN-1505 and DEN-1522.
