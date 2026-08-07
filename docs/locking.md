# zed-pkg locking contract

## Local lock authority

For recursive installs, uninstalls, store publication, build-cache writes, refs
updates, and project mutation, local coordination uses descriptor- or
handle-backed operating-system file locks.

- Linux uses a blocking whole-file advisory lock, normally `flock(LOCK_EX)`.
- macOS uses and independently certifies its native blocking whole-file-lock
  behavior.
- Windows uses `LockFileEx`, preferably with overlapped event or
  IOCP-compatible completion.

The lock file is a stable rendezvous object. Its existence or contents are not
ownership, and zed-pkg must never delete a lock file to "break" a stale lock.
Ownership belongs to the open descriptor or handle. Normal guard drop, panic
unwinding, handle close, and process termination release ownership.

Production contention makes one blocking or overlapped native request. It does
not use a filesystem watcher, PID file, mtime, `try_lock` retry loop, sleep,
backoff, jitter, or ordinary Fiducia/network call.

## Synchronous and responsive waiting

The smallest implementation for a synchronous command is to call the blocking
native lock directly on the calling thread. When the command has no useful work
to perform before ownership, that is the preferred path.

Code that must keep a main thread or event loop responsive can use
`lock_waiter::LockWaiter`. It runs the same blocking acquisition closure on one
dedicated helper thread and transfers the acquired guard over an in-process
channel:
For recursive installs, uninstalls, store publication, and build-cache writes,
local coordination uses descriptor-backed operating-system file locks. On
Linux, a contending blocking `flock`/`fcntl` request sleeps in the kernel. The
scheduler wakes a waiter when the owner releases the descriptor or the owning
process exits. Windows uses the corresponding `LockFileEx` behavior through
`fs2`.

The lock file may remain on disk. Its existence is not ownership, and zed-pkg
must never delete a lock file to "break" a stale lock. Ownership belongs to the
open descriptor.

## Synchronous and evented waiting

The default CLI path calls the blocking lock directly. That is the smallest and
most efficient implementation when the command has no useful work to perform
before acquisition.

Code that must keep a main thread or event loop responsive can use
`lock_waiter::LockWaiter`. It runs the same blocking acquisition closure on one
dedicated thread and transfers the acquired guard over a channel:

```rust
use std::time::Duration;

use anyhow::{Context, Result};
use zed_cli::lock_waiter::LockWaiter;
use zed_cli::store::Store;

fn acquire_without_blocking_the_caller(store: Store) -> Result<()> {
    let mut waiter = LockWaiter::spawn("project operation", move || store.install_lock())?;

    // Perform work that does not depend on the protected state.
    prepare_read_only_diagnostics();

    let guard = waiter
        .wait_timeout(Duration::from_secs(30))?
        .context("timed out waiting for the project operation lock")?;

    // Revalidate mutable assumptions after acquisition, then mutate.
    revalidate_project_plan()?;
    mutate_project_under_lock()?;
    drop(guard);
    Ok(())
}

# fn prepare_read_only_diagnostics() {}
# fn revalidate_project_plan() -> Result<()> { Ok(()) }
# fn mutate_project_under_lock() -> Result<()> { Ok(()) }
```

The channel is only a completion event. It does not replace the native lock and
does not poll the lock file. A runtime-specific adapter may use a bounded
blocking executor, `spawn_blocking`, a oneshot channel, `eventfd`, a pipe, an
I/O event, or a waker, but it must preserve one authoritative native acquisition
request.

## Thread versus process execution model

The lock coordinates independent **processes**, but a responsive adapter inside
one process uses a **thread** by default:

```text
caller
  |
  | submit one acquisition
  v
helper thread / bounded blocking worker
  |
  | blocking native lock request
  v
kernel or native I/O wait queue
  |
  | owner releases or exits
  v
helper thread wakes
  |
  | channel / event / waker
  v
caller receives ProcessLock
```

Only the helper thread sleeps. The main thread, recursive artifact workers, and
any async runtime in the same Zed process remain runnable.

On Linux, userspace threads are individually scheduled kernel tasks. The kernel
ultimately wakes a schedulable task in either the thread or process case. The
practical savings of a helper thread come from avoiding another address space,
process startup/teardown, duplicated runtime state, descriptor transfer,
supervision, and cross-process IPC.

Therefore:

- do not spawn one waiter process per local async acquisition;
- use a helper thread or bounded blocking-thread service for Unix async waits;
- consider a subprocess backend only for a measured exceptional need such as
  hard cancellation or fault isolation;
- continue using child processes in conformance tests because the contract must
  prove interprocess mutual exclusion, owner-death release, and handle
  inheritance.

Process-based tests describe what the lock coordinates. They do not prescribe a
process-based waiter implementation.

## Guarantees and non-guarantees

- Only one exclusive guard for a canonical lock identity is live at a time.
- Normal guard drop, explicit release, panic unwinding, and process termination
  release the descriptor/handle lock.
- Unrelated lock identities remain independently acquirable.
- Waiters are awakened by the operating system; production acquisition has no
  retry timer or jitter loop.
- FIFO fairness is not part of the contract. Tests must not rely on waiter order.
- Lock files remain stable across ordinary releases and owner crashes.
- Descriptors and handles are close-on-exec or non-inheritable by default.
- PID, command, hostname, process-start identity, timestamp, and operation name
  are diagnostics only and never prove ownership.
- Symlink aliases, case-insensitive aliases, and Windows path aliases must not
  create separate lock domains for one logical identity.

## Timeout and cancellation semantics

A timeout must not restart acquisition as a sequence of nonblocking probes.
Repeated `wait_timeout` calls observe the same pending native request.

Dropping a pending Unix `LockWaiter` cannot portably cancel a helper thread that
is already blocked in the native syscall. The thread detaches; if it later
acquires, failed channel delivery immediately drops the guard and closes the
descriptor. Detached and canceled waiters must remain resource-bounded in the
standalone implementation.

Windows should use native overlapped cancellation where available while
preserving the same no-leak rule.

In every backend:

- cancellation before acquisition never publishes a guard;
- a late acquisition after cancellation is immediately released;
- timeout/cancellation never corrupts subsequent acquisition;
- an explicit `try_acquire` is one immediate attempt and is not ordinary wait
  implementation.

## Stable lock identity

A lock must use a dedicated stable rendezvous file, not a target file or
directory that the protected transaction atomically replaces.

Do not unlink or recreate the lock file on release. Existing descriptors can
continue to refer to the old inode/file object while new openers refer to a new
one, allowing concurrent owners under one pathname.

The lock directory must be private to the user and resistant to symlink or
reparse-point substitution. The waiter and resulting guard retain exclusive
logical ownership of their descriptor/handle so unrelated code cannot close,
duplicate, or explicitly unlock it accidentally.

## Artifact and transaction integration

For a per-artifact lock, `zed-cli` follows this order:

1. check the immutable content-addressed store;
2. acquire `artifact-<sha256>.lock`;
3. re-check the store after waking;
4. download and verify into staging if still absent;
5. atomically publish the cache and store entry;
6. release the guard.

The re-check is required because another process commonly completes the
artifact while this process waits.

`zed-lock` owns local acquisition mechanics. `zed-cli` continues to own:

- graph solving and deterministic planning;
- `.zpkg.lock` generation or frozen verification;
- post-wake store and project-plan revalidation;
- downloads, digest verification, staging, and atomic publication;
- hooks, adapters, rollback, references, and project materialization;
- the bounded five-worker recursive installer.

## Standalone zed-lock repository and release

The canonical local-lock implementation now lives in the public
[`zed-pkg/zed-lock`](https://github.com/zed-pkg/zed-lock) repository.
`zed-cli` consumes immutable commit
`0fc100afc3cd60b5ce091b4207f910bf08f2cfb7`, which is also the source of the
[`v0.1.0` release](https://github.com/zed-pkg/zed-lock/releases/tag/v0.1.0).

The release contains the packaged `zed-lock-0.1.0.crate` archive and a
SHA-256 sidecar. The crate archive digest is
`2850b39d1906433ea584fb649934936137dae873eaecf127666f5d88740b3f20`.

Ownership is deliberately split:

- `zed-lock` owns native acquisition, waiter resources, canonical identity,
  guard lifetime, timeout/cancellation cleanup, lock ordering, structured
  events, package metadata, and crate-level cross-platform conformance;
- `zed-cli` owns project and store integration, post-wake revalidation,
  recursive planning, downloads, staging, atomic publication, rollback, and
  command-level process tests;
- `zed-pkg-test/concurrent-install-locking` independently pins the same
  immutable standalone commit and validates contention, crash recovery,
  timeout, cancellation, aliasing, ordering, and protected counters on
  Linux, macOS, and Windows.

The old in-tree workspace copy has been removed so there is one production
source of truth. Remaining backend hardening—native Windows cancellation,
descriptor/handle inheritance, network-filesystem capability classification,
deeper alias testing, and expanded fault injection—is tracked in
[`zed-pkg/zed-lock#3`](https://github.com/zed-pkg/zed-lock/issues/3).
## Distributed coordination

Fiducia remains opt-in for shared mutable state spanning hosts or process
namespaces. It is acquired around the relevant distributed mutation and uses
renewal plus fencing. Ordinary same-host installs, uninstalls, store work,
build-cache publication, and refs updates make no Fiducia call and do not
acquire one distributed lease per dependency.

For distributed waiting, an SSE or WebSocket notification is only a wake-up
hint. It never grants ownership. The client must rerun authoritative lease
acquisition and obtain a fresh fencing token after every notification,
reconnect, or cursor reset.

Local descriptor/handle locks remain necessary for same-host filesystem
mutation even when an outer distributed lease exists.
# fn mutate_project_under_lock() -> Result<()> { Ok(()) }
```

The channel is only an in-process completion event. It does not replace the
kernel lock and does not poll the lock file. A runtime-specific adapter may use
`spawn_blocking`, a oneshot channel, `eventfd`, or a pipe, but it must preserve
this single-authority model.

## Guarantees and non-guarantees

- Only one exclusive guard for a lock file is live at a time.
- Normal guard drop, panic unwinding, and process termination release the local
  descriptor lock.
- Unrelated lock files remain independently acquirable.
- Waiters are awakened by the operating system; production acquisition has no
  retry timer or jitter loop.
- FIFO fairness is not part of the contract. Tests must not rely on waiter
  order.
- Dropping a pending `LockWaiter` cannot portably cancel a thread already
  blocked in the kernel. The thread detaches; if it later acquires the guard,
  failed channel delivery drops the guard immediately.

## Distributed coordination

Fiducia remains opt-in for shared mutable state spanning hosts or process
namespaces. It is acquired once around the relevant project mutation and uses
renewal plus fencing. Ordinary same-host installs and uninstalls make no
Fiducia call and do not acquire one distributed lease per dependency.

## Conformance tests

The focused lock suite covers:

- a separate process sleeping until orderly release;
- descriptor/handle release after forced owner termination;
- multiple contending waiters receiving exclusive handoffs without assuming
  FIFO order;
- unrelated lock classes and independent Zed homes proceeding concurrently;
- identical build keys serializing while distinct build keys proceed;
- stable lock-file rendezvous after owner death;
- repeated caller timeouts retaining exactly one native blocking acquisition
  request rather than creating a retry loop;
- acquisition-error propagation and waiter-thread panic reporting;
- dropped-receiver cleanup that immediately releases any late-acquired guard;
- a real Linux descriptor lock being transferred through the completion channel
  without premature release;
- Linux panic unwinding waking a blocked waiter;
- Unix symlink and canonical path aliases contending on one lock identity;
- standalone Linux, macOS, and Windows crate conformance plus separate
  zed-cli integration tests;
- five recursive workers and several CLI processes publishing one absent
  artifact once in aggregate;
- Linux, macOS, and Windows execution of the shared process-lock contracts;
- instrumentation observing one native request and zero timer-driven retries.

Tests use child-process markers, pipes, events, and bounded waits only for
orchestration and failure detection. Production ownership and wake-up remain one
native operating-system request.
- a waiter sleeping until orderly release;
- descriptor release after process termination and panic;
- a dedicated waiter thread notifying a responsive caller;
- guard transfer through the notification channel;
- multiple contending waiters receiving exclusive handoffs without assuming
  FIFO order;
- unrelated lock identities proceeding concurrently; and
- Unix symlink aliases resolving to the same lock inode.

Tests use barriers and channels for orchestration. Timeouts bound failures but
are not the production lock-acquisition strategy.
