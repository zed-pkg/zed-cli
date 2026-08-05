# zed-pkg locking contract

## Local lock authority

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
    mutate_project_under_lock()?;
    drop(guard);
    Ok(())
}

# fn prepare_read_only_diagnostics() {}
# fn mutate_project_under_lock() -> Result<()> { Ok(()) }
```

The channel is only an in-process completion event. It does not replace the
kernel lock and does not poll the lock file. A runtime-specific adapter may use
`spawn_blocking`, a oneshot channel, `eventfd`, or a pipe, but it must preserve
this single-authority model.

## Guarantees and non-guarantees

- Only one exclusive guard for a lock identity is live at a time.
- Normal guard drop and process termination release the local descriptor lock.
- Unrelated lock identities remain independently acquirable.
- Waiters are awakened by the operating system; production acquisition has no
  retry timer or jitter loop.
- FIFO fairness is not part of the contract. Tests must not rely on waiter
  order.
- Dropping a pending `LockWaiter` cannot portably cancel a thread already
  blocked in the kernel. The thread detaches; if it later acquires the guard,
  failed channel delivery drops the guard immediately.
- A `wait_timeout` result of `None` does not restart acquisition. The original
  waiter remains blocked in the kernel and can be awaited again.

## Distributed coordination

Fiducia remains opt-in for shared mutable state spanning hosts or process
namespaces. It is acquired once around the relevant project mutation and uses
renewal plus fencing. Ordinary same-host installs and uninstalls make no
Fiducia call and do not acquire one distributed lease per dependency.

For distributed waiting, an SSE or WebSocket notification is only a wake-up
hint. It never grants ownership. The client must rerun authoritative lease
acquisition and obtain a fresh fencing token after every notification,
reconnect, or cursor reset.

## Conformance tests

The focused lock suite covers:

- a separate process sleeping until orderly release;
- descriptor release after forced owner termination;
- multiple contending waiters receiving exclusive handoffs without assuming
  FIFO order;
- unrelated lock classes and independent Zed homes proceeding concurrently;
- identical build keys serializing while distinct build keys proceed;
- stable lock-file rendezvous after owner death;
- waiter timeout reuse, acquisition-error propagation, waiter-thread panic
  reporting, and dropped-receiver guard cleanup; and
- Linux, macOS, and Windows execution of the process-lock contracts.

Tests use child-process markers and bounded waits only for orchestration and
failure detection. Production lock acquisition remains one blocking operating-
system request rather than a userspace retry loop.
