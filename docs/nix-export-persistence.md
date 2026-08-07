# Atomic standalone Nix bundle persistence

Tracking: DEN-1592, DEN-1418, DEN-1422, DEN-1508

The pure `zed.nix-flake-bundle/v1` renderer remains an in-memory, read-only
boundary. Persistence is a separate authority implemented by
`persist_nix_export_bundle`.

## Fresh publication

A caller supplies one validated `RenderedNixExportBundle` and an explicit output
directory whose immediate parent already exists. The persistence layer:

1. validates the complete in-memory bundle and rejects `..` traversal;
2. requires the immediate output parent to be a real directory rather than a
   symbolic link;
3. canonicalizes that validated parent once, allowing operating-system or
   administrator aliases above it such as macOS `/var` → `/private/var`;
4. performs all later inspection, staging, rename, and synchronization through
   that canonical parent;
5. rejects a symbolic-link destination;
6. creates one fresh hidden sibling directory on the destination filesystem;
7. creates every file exclusively with canonical mode `0644` on Unix;
8. synchronizes every file and staged directory;
9. re-reads the complete staged tree and compares its exact file set, bytes, and
   modes with the renderer output; and
10. publishes with one atomic rename followed by parent-directory
    synchronization.

The distinction between an ancestor alias and a symlink supplied as the leaf
parent is intentional. Rejecting every lexical ancestor incorrectly rejects
ordinary macOS temporary paths below `/var`; accepting an immediate parent
symlink would let a caller redirect the publication boundary. Tests cover both
cases on Unix.

A failure before the final rename removes the complete temporary tree and never
creates the final destination.

## No-clobber and idempotence

Persistence never overwrites an existing destination.

An existing real directory is accepted only when it has the exact rendered file
set, exact bytes, and canonical file modes. That path returns
`AlreadyCurrent` without rewriting content. Existing files, destination
symlinks, unrelated directories, extra files, missing files, changed bytes, or
changed modes fail unchanged.

Concurrent identical writers are safe: exactly one writer publishes. A losing
writer verifies the winner's complete output and returns `AlreadyCurrent`; a
non-identical winner causes a fail-closed error.

Strict version 1 has no force or overwrite escape hatch.

## Security boundary

Persistence does not:

- invoke Nix;
- execute package hooks, build scripts, or generated binaries;
- read registry, signing, cloud, Cachix, or Attic credentials;
- access user-home package state;
- update a lock;
- realize a store output;
- sign or publish an overlay, cache, registry record, or OCI image.

Command routing, canonical JSON user output, and explicit planner/renderer input
selection remain the next slice of DEN-1592. The filesystem contract is kept in a
public library API so command tests and external clean-room tests use the same
implementation rather than duplicating persistence logic.
