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

Downloads are written to a temporary file in the cache directory, verified,
and atomically renamed. A corrupt cache file left by an interrupted older
client is removed and fetched again while the artifact lock is held.

## Materialization

The concurrent phase only warms the global store. The existing project
transaction remains responsible for lockfile writes, adapter wiring, rollback,
and dependency materialization. Unix installations therefore continue to use
store-backed symlinks by default; explicit copy mode and the non-Unix fallback
remain self-contained copies.
