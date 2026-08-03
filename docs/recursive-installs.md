# Recursive and concurrent installs

`zed install` resolves dependencies from every installed package manifest, not
only the consumer's direct dependency table. The resolver keeps one selected
version per `org/name`, detects incompatible requirements, terminates cycles,
and deduplicates diamond-shaped graphs before the project tree is changed.

## Concurrency

Artifact download, integrity verification, and extraction run through a bounded
worker queue. The default is five concurrent artifact installs:

```text
ZED_PKG_INSTALL_CONCURRENCY=5 zed install
```

`ZED_PKG_INSTALL_CONCURRENCY` may be set from 1 through 32. Invalid values and
zero use the default. Registry metadata resolution stays deterministic on the
coordinator thread; only independent artifact work is concurrent.

## Cross-process locking

Each content hash owns a lock file under:

```text
$ZED_PKG_HOME/locks/artifact-<sha256>.lock
```

The lock uses the operating system's blocking file-lock primitive. A waiter
sleeps in the kernel until the owner exits or releases the descriptor; there is
no retry timer or filesystem polling loop. The lock covers download, SHA-256
verification, atomic publication into the download cache, and extraction into
the content-addressed store. Two terminals or CI jobs sharing a Zed home can
therefore install unrelated artifacts concurrently while downloading any one
artifact at most once.

Recursive prefetch and the transactional installer use the same artifact
acquisition function. Frozen installs, build preparation, `zed add`, and
`zed remove` therefore cannot bypass the per-hash lock through an older direct
cache-download path.

Downloads are written to a temporary file in the cache directory, verified,
and atomically renamed. A corrupt cache file left by an interrupted older
client is removed and fetched again while the artifact lock is held.

## Materialization

The concurrent phase only warms the global store. The existing project
transaction remains responsible for lockfile writes, adapter wiring, rollback,
and dependency materialization. Unix installations therefore continue to use
store-backed symlinks by default; explicit copy mode and the non-Unix fallback
remain self-contained copies.

## Frozen and workspace compatibility

Recursive acquisition is composed with the strict frozen-lock boundary. A
frozen install may verify artifacts, repair deleted links, reconstruct workspace
projections, and refresh generated adapter wiring, but it preserves the caller's
committed lockfile bytes exactly. It does not rewrite comments, provenance, or
fields introduced by a newer CLI.

Workspace packages still enter the ordinary adapter and transaction lifecycle
through the `ops_entry` dispatcher. This keeps direct and transitive workspace
links, copy mode, uninstall cleanup, and post-uninstall frozen restoration on
the same fail-closed path as registry packages.

The recursive install modules coexist with the resolver-only frozen fetch path,
Nix fixed-output bundle generation, canonical export planning, and standalone
pure flake rendering. Those export surfaces consume the same validated lock and
content identities; they do not bypass recursive integrity checks or per-hash
locking.

## Validation

The unit suite races the recursive and transactional acquisition paths against
the same absent content hash. The first download is held open while the second
path attempts acquisition; the test proves that the second path never enters
its registry download and that the aggregate download count remains one.

A separate parent/child-process regression holds an artifact lock in the parent,
proves that the child remains blocked after reaching acquisition, releases the
owner descriptor, and then requires the child to wake and acquire successfully.
The same contract runs on Windows together with the shared-home deduplication,
mixed recursive/transactional acquisition, and failed-download cleanup tests.

The companion end-to-end suite also runs multiple independent CLI processes
against one shared Zed home, verifies the complete transitive lock graph, and
checks that project packages are store-backed symlinks rather than copied
package trees.
