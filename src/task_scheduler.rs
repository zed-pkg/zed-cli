//! Preliminary bounded scheduler for the native task runtime.
//!
//! This module is intentionally not wired into `TaskRuntime` yet. The current
//! runtime recursively executes task dependencies and parallel groups; replacing
//! its scoped thread batches with a fixed pool without handling nested submission
//! can deadlock. The draft scheduler therefore makes same-scheduler nested work
//! execute inline on the current worker while external producers use a bounded
//! queue with backpressure.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

type Job = Box<dyn FnOnce() + Send + 'static>;

static NEXT_SCHEDULER_ID: AtomicUsize = AtomicUsize::new(1);

std::thread_local! {
    static CURRENT_SCHEDULER: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Resource limits for one task-run scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    worker_count: NonZeroUsize,
    queue_capacity: NonZeroUsize,
}

impl SchedulerConfig {
    #[must_use]
    pub const fn new(worker_count: NonZeroUsize, queue_capacity: NonZeroUsize) -> Self {
        Self {
            worker_count,
            queue_capacity,
        }
    }

    /// Preserve the current `--jobs N` compatibility contract while deriving a
    /// small finite queue. A later policy change can make queue capacity explicit
    /// without changing the scheduler implementation.
    #[must_use]
    pub fn from_jobs(jobs: usize) -> Option<Self> {
        let worker_count = NonZeroUsize::new(jobs)?;
        let queue_capacity = NonZeroUsize::new(jobs.saturating_mul(4).max(1))?;
        Some(Self::new(worker_count, queue_capacity))
    }

    #[must_use]
    pub const fn worker_count(self) -> usize {
        self.worker_count.get()
    }

    #[must_use]
    pub const fn queue_capacity(self) -> usize {
        self.queue_capacity.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Closed,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("task scheduler is closed"),
        }
    }
}

impl std::error::Error for SubmitError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobJoinError {
    Panicked,
    ResultChannelClosed,
}

impl fmt::Display for JobJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked => formatter.write_str("scheduled task panicked"),
            Self::ResultChannelClosed => {
                formatter.write_str("scheduled task result channel closed unexpectedly")
            }
        }
    }
}

impl std::error::Error for JobJoinError {}

/// Result handle for one admitted scheduler job.
pub struct JobHandle<T> {
    receiver: mpsc::Receiver<Result<T, JobJoinError>>,
}

impl<T> JobHandle<T> {
    pub fn join(self) -> Result<T, JobJoinError> {
        match self.receiver.recv() {
            Ok(result) => result,
            Err(_) => Err(JobJoinError::ResultChannelClosed),
        }
    }
}

struct SchedulerState {
    queue: VecDeque<Job>,
    accepting: bool,
    queue_capacity: usize,
}

struct SharedScheduler {
    id: usize,
    state: Mutex<SchedulerState>,
    work_ready: Condvar,
    space_ready: Condvar,
}

/// Cloneable submission surface. Workers may safely submit nested work through
/// this handle: same-scheduler nested jobs run inline instead of enqueueing and
/// synchronously waiting behind their own worker pool.
#[derive(Clone)]
pub struct SchedulerHandle {
    shared: Arc<SharedScheduler>,
}

impl SchedulerHandle {
    pub fn submit<F, T>(&self, job: F) -> Result<JobHandle<T>, SubmitError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        let wrapped: Job = Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(job))
                .map_err(|_| JobJoinError::Panicked);
            let _ = sender.send(result);
        });

        let nested_on_same_scheduler =
            CURRENT_SCHEDULER.with(|current| current.get() == Some(self.shared.id));

        if nested_on_same_scheduler {
            let accepting = self
                .shared
                .state
                .lock()
                .expect("task scheduler mutex poisoned")
                .accepting;
            if !accepting {
                return Err(SubmitError::Closed);
            }
            wrapped();
            return Ok(JobHandle { receiver });
        }

        let mut state = self
            .shared
            .state
            .lock()
            .expect("task scheduler mutex poisoned");
        while state.accepting && state.queue.len() >= state.queue_capacity {
            state = self
                .shared
                .space_ready
                .wait(state)
                .expect("task scheduler mutex poisoned");
        }
        if !state.accepting {
            return Err(SubmitError::Closed);
        }
        state.queue.push_back(wrapped);
        self.shared.work_ready.notify_one();
        Ok(JobHandle { receiver })
    }

    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("task scheduler mutex poisoned")
            .queue
            .len()
    }

    #[must_use]
    pub fn queue_capacity(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("task scheduler mutex poisoned")
            .queue_capacity
    }
}

/// Owns the fixed worker set for one task-run invocation.
pub struct BoundedScheduler {
    shared: Arc<SharedScheduler>,
    workers: Vec<JoinHandle<()>>,
    worker_count: usize,
}

impl BoundedScheduler {
    pub fn new(config: SchedulerConfig) -> std::io::Result<Self> {
        let shared = Arc::new(SharedScheduler {
            id: NEXT_SCHEDULER_ID.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(SchedulerState {
                queue: VecDeque::with_capacity(config.queue_capacity()),
                accepting: true,
                queue_capacity: config.queue_capacity(),
            }),
            work_ready: Condvar::new(),
            space_ready: Condvar::new(),
        });

        let mut workers = Vec::with_capacity(config.worker_count());
        for index in 0..config.worker_count() {
            let worker_shared = Arc::clone(&shared);
            match thread::Builder::new()
                .name(format!("zed-task-worker-{index}"))
                .spawn(move || worker_loop(worker_shared))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    close_scheduler(&shared);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            }
        }

        Ok(Self {
            shared,
            workers,
            worker_count: config.worker_count(),
        })
    }

    #[must_use]
    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    #[must_use]
    pub const fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Stop admitting new external work, drain already-admitted jobs, and join
    /// every worker. Dropping the scheduler has the same semantics.
    pub fn shutdown(mut self) {
        self.close_and_join();
    }

    fn close_and_join(&mut self) {
        close_scheduler(&self.shared);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for BoundedScheduler {
    fn drop(&mut self) {
        self.close_and_join();
    }
}

fn close_scheduler(shared: &SharedScheduler) {
    let mut state = shared
        .state
        .lock()
        .expect("task scheduler mutex poisoned");
    state.accepting = false;
    shared.work_ready.notify_all();
    shared.space_ready.notify_all();
}

fn worker_loop(shared: Arc<SharedScheduler>) {
    CURRENT_SCHEDULER.with(|current| {
        let previous = current.replace(Some(shared.id));
        worker_loop_inner(&shared);
        current.set(previous);
    });
}

fn worker_loop_inner(shared: &SharedScheduler) {
    loop {
        let next_job = {
            let mut state = shared
                .state
                .lock()
                .expect("task scheduler mutex poisoned");
            loop {
                if let Some(job) = state.queue.pop_front() {
                    shared.space_ready.notify_one();
                    break Some(job);
                }
                if !state.accepting {
                    break None;
                }
                state = shared
                    .work_ready
                    .wait(state)
                    .expect("task scheduler mutex poisoned");
            }
        };

        match next_job {
            Some(job) => job(),
            None => return,
        }
    }
}
