# Formal-methods change procedure

`zed` resolves versions, writes lockfiles, installs content-addressed artifacts, links project trees, executes opt-in builds, publishes provenance-bound releases, and rotates credentials. Failures can corrupt reproducibility, execute unintended code, or publish the wrong source. This procedure defines how those state machines are modeled and reviewed; it does **not** claim the planned models already exist.

The checked inventory is [`procedure.toml`](procedure.toml).

## Change procedure

1. Identify the affected machine before changing version resolution, lockfile semantics, store insertion, project linking, manifestless inference/confirmation, build permission, publishing, VCS provenance, auth refresh, garbage collection, or self-update.
2. Separate durable facts: manifest, lockfile, immutable source artifact, content hash, store entry, project link, build cache, VCS tag/commit, registry publication, and credential generation.
3. Model filesystem operations as staged transitions. A crash between download, hash verification, atomic rename, lockfile write, link replacement, or publish acknowledgement must not expose a falsely complete state.
4. State safety first. Liveness claims require assumptions about an available registry/VCS host, terminating filesystem operations, eventual retry, and explicit user approval for builds or manifestless installs.
5. Use finite Quint/TLC or Apalache models for resolver/install/publish schedules and property testing for version algebra. Replay traces against production Rust using temporary filesystems, deterministic registry/VCS fixtures, injected faults, and canonical observations.
6. Keep pull-request profiles bounded; widen dependency graphs, cycles, retries, concurrent processes, crash points, and version schemes periodically.
7. Record model/source hashes, registry and VCS fixture revisions, exact graph/bounds, platform/filesystem assumptions, tool versions, and result class.

## Claim language

Use only **typechecked specification**, **randomized exploration**, **bounded exhaustive verification**, **implementation replay**, **differential replay**, or **unbounded proof**. A bounded resolver model is not proof of every registry, filesystem, archive parser, build script, VCS host, or operating system. “Reproducible” claims must name inputs, normalized archive rules, hashes, platform, and whether network metadata was fixed.

## Counterexamples

Retain original and minimized dependency graph, manifests, lockfile, registry responses, VCS refs, filesystem fault schedule, expected/actual state, and revisions. Classify resolver, store, installer, archive, provenance, auth, adapter, or assumption defect; add a deterministic Rust regression and retain minimized traces under `formal/regressions/`. Never ignore an unexpected file, hash, version, confirmation, build, credential, or tag/commit mismatch.

## Required review triggers

Formal review is mandatory for version parsing/range selection, dependency graph/cycle behavior, lockfile identity, store hashing or atomicity, symlink/copy install, manifestless root/adapter inference, confirmation bypass, build opt-in/cache keys, pack pruning/determinism, publish tag/commit checks, yank semantics, auth token rotation/storage, GC reachability, or self-update replacement.

## Initial modeling order

1. **Manifestless install.** Root discovery, ambiguity, interactive confirmation, automation opt-in, lockfile reconstruction, and no unintended manifest creation.
2. **Resolve-lock-store-install.** Version choice, transitive graph, immutable pins, content hash, atomic store insertion, project links, concurrent install, and crash recovery.
3. **Publish provenance.** Clean tree, matching tag at HEAD, deterministic artifact, upload ambiguity, retry, yank, and immutable version identity.
4. **Auth lifecycle.** Access/refresh token generations, refresh rotation, failed refresh, logout/revocation, and credential-file atomicity.
