//! Evented waiting for descriptor-backed operating-system locks.
//!
//! The lock acquisition itself remains a blocking kernel operation. This
//! module only moves that blocking call onto a dedicated thread and reports
//! completion over a channel so a caller can keep its main thread or event
//! loop responsive.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

/// A background waiter for a descriptor-backed lock guard.
///
/// `spawn` runs exactly one acquisition closure on a dedicated thread. The
/// closure should call a blocking operating-system lock primitive, such as
/// [`crate::store::Store::install_lock`]. Once the kernel grants the lock, the
/// acquired guard is transferred to the caller over a channel.
///
/// This is intentionally not a second locking or notification protocol: there
/// is no filesystem watcher, socket, retry loop, backoff, or stale-lock
/// reclamation. The operating system remains authoritative for lock ownership
/// and wake-up.
///
/// Dropping a pending waiter does not portably cancel the kernel-blocked
/// thread. The thread is detached; if it later acquires the lock, channel
/// delivery fails and the guard is dropped immediately.
#[must_use = "dropping a pending lock waiter detaches its kernel-blocked thread"]
pub struct LockWaiter<G> {
    label: String,
    receiver: Receiver<Result<G>>,
    worker: Option<JoinHandle<()>>,
    completed: bool,
}

impl<G: Send + 'static> LockWaiter<G> {
    /// Start one dedicated waiter thread.
    pub fn spawn(
        label: impl Into<String>,
        acquire: impl FnOnce() -> Result<G> + Send + 'static,
    ) -> Result<Self> {
        let label = label.into();
        let thread_name = waiter_thread_name(&label);
        let (sender, receiver) = mpsc::sync_channel(1);

        let worker = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                // If the receiver has been dropped, SendError owns the result;
                // dropping it also drops any acquired guard immediately.
                let _ = sender.send(acquire());
            })
            .with_context(|| format!("spawning background lock waiter for `{label}`"))?;

        Ok(Self {
            label,
            receiver,
            worker: Some(worker),
            completed: false,
        })
    }

    /// Block the caller until the kernel grants the lock or acquisition fails.
    pub fn wait(mut self) -> Result<G> {
        self.ensure_pending()?;
        match self.receiver.recv() {
            Ok(acquisition) => self.finish(acquisition),
            Err(_) => self.disconnected(),
        }
    }

    /// Wait for an acquisition event for at most `timeout`.
    ///
    /// A timeout does not cancel or restart lock acquisition. The dedicated
    /// thread remains asleep inside the kernel lock call and this waiter may be
    /// used again.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<G>> {
        self.ensure_pending()?;
        match self.receiver.recv_timeout(timeout) {
            Ok(acquisition) => self.finish(acquisition).map(Some),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => self.disconnected(),
        }
    }

    fn ensure_pending(&self) -> Result<()> {
        if self.completed {
            bail!(
                "background lock waiter for `{}` already completed",
                self.label
            );
        }
        Ok(())
    }

    fn finish(&mut self, acquisition: Result<G>) -> Result<G> {
        self.completed = true;
        self.join_worker()?;
        acquisition
    }

    fn disconnected<T>(&mut self) -> Result<T> {
        self.completed = true;
        match self.join_worker() {
            Ok(()) => Err(anyhow!(
                "background lock waiter for `{}` ended without reporting a result",
                self.label
            )),
            Err(error) => Err(error),
        }
    }

    fn join_worker(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| anyhow!("background lock waiter for `{}` panicked", self.label))
    }
}

impl<G> Drop for LockWaiter<G> {
    fn drop(&mut self) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn waiter_thread_name(label: &str) -> String {
    let suffix: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    if suffix.is_empty() {
        "zed-lock-waiter".to_owned()
    } else {
        format!("zed-lock-{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use anyhow::{Result, anyhow};

    use super::LockWaiter;

    #[test]
    fn timeout_keeps_the_same_waiter_live() -> Result<()> {
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let mut waiter = LockWaiter::spawn("unit-timeout", move || {
            release_receiver
                .recv()
                .map_err(|_| anyhow!("release channel closed"))?;
            Ok(42_u8)
        })?;

        assert_eq!(
            waiter.wait_timeout(Duration::from_millis(20))?,
            None,
            "a timeout must not synthesize an acquisition event"
        );
        release_sender.send(()).unwrap();
        assert_eq!(
            waiter.wait_timeout(Duration::from_secs(1))?,
            Some(42),
            "the original waiter should still deliver the eventual result"
        );
        Ok(())
    }

    #[test]
    fn repeated_timeouts_do_not_repeat_the_native_acquisition_request() -> Result<()> {
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let mut waiter = LockWaiter::spawn("unit-single-request", move || {
            worker_attempts.fetch_add(1, Ordering::SeqCst);
            started_sender
                .send(())
                .map_err(|_| anyhow!("start receiver closed"))?;
            release_receiver
                .recv()
                .map_err(|_| anyhow!("release channel closed"))?;
            Ok(())
        })?;

        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| anyhow!("waiter did not start its acquisition request"))?;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            waiter.wait_timeout(Duration::from_millis(20))?.is_none(),
            "the blocked acquisition completed before release"
        );
        assert!(
            waiter.wait_timeout(Duration::from_millis(20))?.is_none(),
            "the blocked acquisition completed before release"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "timeouts must not turn one blocking request into a retry loop"
        );

        release_sender.send(()).unwrap();
        assert!(
            waiter.wait_timeout(Duration::from_secs(1))?.is_some(),
            "the original request did not deliver its completion event"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn acquisition_errors_are_forwarded() {
        let waiter: LockWaiter<()> =
            LockWaiter::spawn("unit-error", || Err(anyhow!("lock denied"))).unwrap();
        let error = waiter.wait().unwrap_err();
        assert!(error.to_string().contains("lock denied"));
    }

    #[test]
    fn waiter_thread_panics_are_reported() {
        let waiter: LockWaiter<()> = LockWaiter::spawn("unit-panic", || -> Result<()> {
            panic!("intentional waiter panic");
        })
        .unwrap();
        let error = waiter.wait().unwrap_err();
        assert!(error.to_string().contains("panicked"));
    }

    #[test]
    fn dropping_a_pending_waiter_drops_any_eventual_guard() -> Result<()> {
        struct DropProbe(mpsc::SyncSender<()>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let (dropped_sender, dropped_receiver) = mpsc::sync_channel(1);
        let waiter = LockWaiter::spawn("unit-detach", move || {
            release_receiver
                .recv()
                .map_err(|_| anyhow!("release channel closed"))?;
            Ok(DropProbe(dropped_sender))
        })?;

        drop(waiter);
        release_sender.send(()).unwrap();
        dropped_receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| anyhow!("detached waiter retained its eventual guard"))?;
        Ok(())
    }
}
