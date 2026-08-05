# zed-lock

`zed-lock` is the local locking core for recursive zed-pkg mutations and shared
artifact publication. It keeps descriptor-backed operating-system locking as
the sole local ownership authority while exposing synchronous, timeout, and
runtime-neutral future APIs.

## Design

- Linux and macOS use the blocking descriptor lock provided through `fs2`.
  A contending waiter sleeps in the kernel until the owner releases the
  descriptor or exits.
- Windows uses the corresponding `LockFileEx` implementation through `fs2`.
- Async callers get one bounded dedicated waiter thread and a completion
  `Future`; there is no `try_lock` plus sleep/backoff polling loop.
- Lock files are stable rendezvous points. Their contents are diagnostics only
  and must never be interpreted as ownership.
- The default same-process policy rejects accidental reentrant acquisition.
  Intentional independent tasks may opt into kernel queuing.
- Multi-lock transactions are canonicalized, deduplicated, sorted by lock
  class and path, and released in reverse order.
- Fiducia is not part of the local path. It remains an optional outer lease and
  fencing layer for genuinely multi-host shared state.

## Example

```rust
use std::time::Duration;

use anyhow::{Context, Result};
use zed_lock::{LockClass, LockManager, LockRequest};

fn mutate() -> Result<()> {
    let request = LockRequest::exclusive(".zed/locks/install.lock")
        .operation("recursive install")
        .class(LockClass::ProjectMutation);

    let guard = LockManager::global()
        .acquire_timeout(request, Duration::from_secs(120))
        .context("waiting for recursive install ownership")?;

    // Revalidate the mutation plan, then commit protected state.
    drop(guard);
    Ok(())
}
```

For an async runtime, `LockManager::acquire` returns a standard-library
`Future<Output = anyhow::Result<LockGuard>>`. The runtime may await it directly
without requiring Tokio inside this crate.

## Timeout and cancellation

On Unix there is no portable cancellation operation for a thread already
blocked inside `flock`. Dropping or timing out a pending waiter therefore
cancels delivery, not the syscall. If the detached waiter later acquires the
lock, failed delivery immediately drops the guard. The manager limits the
number of such waiter threads to provide backpressure.

## Repository extraction

The crate currently lives under `zed-cli/crates/zed-lock` so the CLI can consume
and validate it immediately. Its package metadata, API boundary, tests, and
`.zpkg.toml` are intentionally self-contained for a history-preserving split to
`github.com/zed-pkg/zed-lock`.
