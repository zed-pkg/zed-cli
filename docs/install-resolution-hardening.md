# Nested, self, symlink, and copy resolution hardening

This document defines the resolver and materialization invariants exercised by
`tests/nested_self_modes.rs` and `.github/workflows/nested-self-install-modes.yml`.
It complements [dependency-solver.md](dependency-solver.md) and
[install-modes.md](install-modes.md).

## Nested dependency graphs

Zed continues to flatten transitive packages into the project modules directory
and record each resolved coordinate once in `.zpkg.lock`. Cycles are legal when
their version requirements are mutually satisfiable; a cycle back-edge does not
recursively expand a package that has already been selected.

The registry-controlled graph is bounded before recursive solving can exhaust
process memory or stack space:

- maximum dependency depth: **256 coordinates** below the root project;
- maximum distinct package coordinates: **10,000**;
- diagnostic provenance paths retain the first four and last eight segments,
  with the omitted segment count rendered explicitly.

A 256-coordinate linear chain must resolve. A 257-coordinate chain must fail
with a deterministic depth-limit diagnostic. The 10,001st distinct coordinate
must fail before it is added to solver state.

## Self-dependencies

Self-dependencies have two supported uses.

### Published package self-loop

A registry package may declare a compatible requirement on its own coordinate,
for example `acme/self-loop@1.0.0 -> acme/self-loop@^1`. The solver selects that
coordinate once, validates the additional constraint, and terminates. An
incompatible self-loop reports a normal conflict with both the consumer path
and the package self-edge path.

### Workspace member testing its published artifact

A workspace member may depend directly on its own package identity in order to
test the artifact consumers actually receive. That direct root self-dependency
is deliberately **not** short-circuited to the workspace source directory. It
is resolved from the configured registry and represented in the lockfile.

Other workspace dependencies remain source-linked. A single test workspace
therefore proves both behaviors together:

- `acme/self-test@2.0.0` installs published `acme/self-test@1.0.0` for its direct
  self-test dependency;
- an ordinary sibling dependency is still materialized from workspace source.

This distinction applies to normal and frozen installs.

## Symlink mode

On Unix, symlink mode writes an absolute canonical directory link. Relative
links whose meaning changes with invocation directory or destination depth are
not emitted. Source and destination are checked for overlap before any existing
output is removed.

On Windows, the requested symlink mode is normalized to copy mode because Zed
does not depend on developer-mode or elevated directory-link privileges.

## Copy mode

Copy mode produces a store-independent, symlink-free package tree suitable for
containers, archives, and moved workspaces.

Before replacing consumer-visible output, Zed stages and validates the complete
copy in the destination parent. Workspace source traversal enforces all of the
following:

- only regular files, directories, and symlinks resolving inside the package
  root are accepted;
- in-package symlinks are dereferenced into ordinary copied files/directories;
- external symlink escapes are rejected;
- directory/symlink cycles are rejected;
- unsupported special filesystem objects are rejected;
- traversal is limited to **200,000 entries** and **256 levels**;
- source/destination equality or containment in either direction is rejected
  before creating missing destination parents;
- a validation or staging failure preserves the previous complete copy.

Lifecycle staging and build-output directory copies use the same guarded copier
rather than a separate recursive implementation.

## Lock-preserving mode transitions

A resolved graph may move between install modes without changing dependency
identity:

1. normal symlink install creates the lock;
2. frozen copy install materializes the same lock as independent directories;
3. deleting the global store does not invalidate the copied tree;
4. a cold frozen copy replay re-fetches the exact locked artifacts;
5. frozen symlink install may materialize the same unchanged lock again.

The CLI integration test compares the lockfile bytes across every transition.

## Verification

Focused local verification:

```sh
cargo test --locked --lib materialize::tests -- --nocapture
cargo test --locked --lib install_graph::solver::tests -- --nocapture
cargo test --locked --test nested_self_modes -- --nocapture
cargo clippy --locked --all-targets -- -D warnings
```

The dedicated GitHub Actions matrix runs the real CLI registry/workspace
round-trips on Linux, macOS, and Windows. Unix additionally exercises source
symlinks, internal symlink dereferencing, external escape rejection, and
failure atomicity.
