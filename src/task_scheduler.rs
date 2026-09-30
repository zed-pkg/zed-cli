//! Bounded scheduler primitive for the native task runtime.
//!
//! This module is intentionally staged ahead of `TaskRuntime` integration. The
//! current runtime recursively executes task dependencies and parallel groups;
//! replacing its scoped batches with a fixed pool without a nested-work progress
//! rule can deadlock. This scheduler keeps external admission bounded while
//! executing same-scheduler nested work inline on the current worker.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type Job = Box<dyn FnOnce() + Send + 'static>;

const QUEUE_MULTIPLIER: usize = 4;
pub(crate) const MAX_WORKER_COUNT: usize = 256;
pub(crate) const MAX_QUEUE_CAPACITY: usize = MAX_WORKER_COUNT * QUEUE_MULTIPLIER;
pub(crate) const MAX_INLINE_NESTING_DEPTH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerContext {
    scheduler_key: Option<usize>,
    inline_depth: usize,
}

std::thread_local! {
    static WORKER_CONTEXT: Cell<WorkerContext> = const {
        Cell::new(WorkerContext {
            scheduler_key: None,
            inline_depth: 0,
        })
    };
}

/// Validated resource limits for one task-run scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchedulerConfig {
    worker_count: NonZeroUsize,
    queue_capacity: NonZeroUsize,
}

impl SchedulerConfig {
    pub(crate) fn new(
        worker_count: usize,
        queue_capacity: usize,
    ) -> Result<Self, SchedulerConfigError> {
        if worker_count == 0 {
            return Err(SchedulerConfigError::ZeroWorkers);
        }
        if worker_count > MAX_WORKER_COUNT {
            return Err(SchedulerConfigError::TooManyWorkers {
                requested: worker_count,
                maximum: MAX_WORKER_COUNT,
            });
        }
        if queue_capacity == 0 {
            return Err(SchedulerConfigError::ZeroQueueCapacity);
        }
        if queue_capacity > MAX_QUEUE_CAPACITY {
            return Err(SchedulerConfigError::QueueTooLarge {
                requested: queue_capacity,
                maximum: MAX_QUEUE_CAPACITY,
            });
        }

        let Some(worker_count) = NonZeroUsize::new(worker_count) else {
            return Err(SchedulerConfigError::ZeroWorkers);
        };
        let Some(queue_capacity) = NonZeroUsize::new(queue_capacity) else {
            return Err(SchedulerConfigError::ZeroQueueCapacity);
        };

        Ok(Self {
            worker_count,
            queue_capacity,
        })
    }

    /// Preserve the current `--jobs N` shape while deriving a small finite queue.
    ///
    /// The absolute ceiling is deliberate: a user-supplied integer must not be
    /// able to request an effectively unbounded number of native threads.
    pub(crate) fn from_jobs(jobs: usize) -> Result<Self, SchedulerConfigError> {
        if jobs > MAX_WORKER_COUNT {
            return Err(SchedulerConfigError::TooManyWorkers {
                requested: jobs,
                maximum: MAX_WORKER_COUNT,
            });
        }
        let queue_capacity = jobs.saturating_mul(QUEUE_MULTIPLIER);
        Self::new(jobs, queue_capacity)
    }

    #[must_use]
    pub(crate) const fn worker_count(self) -> usize {
        self.worker_count.get()
    }

    #[must_use]
    pub(crate) const fn queue_capacity(self) -> usize {
        self.queue_capacity.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerConfigError {
    ZeroWorkers,
    TooManyWorkers { requested: usize, maximum: usize },
    ZeroQueueCapacity,
    QueueTooLarge { requested: usize, maximum: usize },
}

impl fmt::Display for SchedulerConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWorkers => formatter.write_str("task scheduler requires at least one worker"),
            Self::TooManyWorkers { requested, maximum } => write!(
                formatter,
                "task scheduler requested {requested} workers; maximum is {maximum}"
            ),
            Self::ZeroQueueCapacity => {
                formatter.write_str("task scheduler queue capacity must be at least one")
            }
            Self::QueueTooLarge { requested, maximum } => write!(
                formatter,
                "task scheduler requested queue capacity {requested}; maximum is {maximum}"
            ),
        }
    }
}

impl std::error::Error for SchedulerConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitError {
    Closed,
    NestedDepthExceeded { maximum: usize },
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("task scheduler is closed to external submissions"),
            Self::NestedDepthExceeded { maximum } => write!(
                formatter,
                "task scheduler inline nesting depth exceeded maximum of {maximum}"
            ),
        }
    }
}

impl std::error::Error for SubmitError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobJoinError {
    Panicked,
    ResultChannelClosed,
    TimedOut,
}

impl fmt::Display for JobJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panicked => formatter.write_str("scheduled task panicked"),
            Self::ResultChannelClosed => {
                formatter.write_str("scheduled task result channel closed unexpectedly")
            }
            Self::TimedOut => formatter.write_str("timed out waiting for scheduled task result"),
        }
    }
}

impl std::error::Error for JobJoinError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchedulerShutdownError {
    panicked_workers: usize,
}

impl SchedulerShutdownError {
    #[must_use]
    pub(crate) const fn panicked_workers(self) -> usize {
        self.panicked_workers
    }
}

impl fmt::Display for SchedulerShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} task scheduler worker(s) panicked during shutdown",
            self.panicked_workers
        )
    }
}

impl std::error::Error for SchedulerShutdownError {}

/// Result handle for one admitted scheduler job.
pub(crate) struct JobHandle<T> {
    receiver: mpsc::Receiver<Result<T, JobJoinError>>,
}

impl<T> JobHandle<T> {
    pub(crate) fn join(self) -> Result<T, JobJoinError> {
        match self.receiver.recv() {
            Ok(result) => result,
            Err(_) => Err(JobJoinError::ResultChannelClosed),
        }
    }

    /// Wait for a result without turning timeout into cancellation.
    ///
    /// A timed-out job remains owned by the scheduler and may still complete.
    pub(crate) fn join_timeout(self, timeout: Duration) -> Result<T, JobJoinError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(JobJoinError::TimedOut),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(JobJoinError::ResultChannelClosed),
        }
    }
}

struct SchedulerState {
    queue: VecDeque<Job>,
    external_admission_open: bool,
}

struct SharedScheduler {
    state: Mutex<SchedulerState>,
    queue_capacity: usize,
    work_ready: Condvar,
    space_ready: Condvar,
}

/// Cloneable submission surface.
///
/// External producers use the bounded queue and block under backpressure.
/// Same-scheduler nested submissions execute inline so a fully occupied fixed
/// pool cannot deadlock waiting for child work that needs that same pool.
#[derive(Clone)]
pub(crate) struct SchedulerHandle {
    shared: Arc<SharedScheduler>,
}

impl SchedulerHandle {
    pub(crate) fn submit<F, T>(&self, job: F) -> Result<JobHandle<T>, SubmitError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let context = WORKER_CONTEXT.with(Cell::get);
        let key = scheduler_key(&self.shared);
        let nested_on_same_scheduler = context.scheduler_key == Some(key);

        if nested_on_same_scheduler && context.inline_depth >= MAX_INLINE_NESTING_DEPTH {
            return Err(SubmitError::NestedDepthExceeded {
                maximum: MAX_INLINE_NESTING_DEPTH,
            });
        }

        let (sender, receiver) = mpsc::sync_channel(1);
        let wrapped: Job = Box::new(move || {
            let result =
                panic::catch_unwind(AssertUnwindSafe(job)).map_err(|_| JobJoinError::Panicked);
            let _send_result = sender.send(result);
        });

        if nested_on_same_scheduler {
            // Shutdown closes new external admission, but work that was already
            // admitted must be able to finish its required nested work.
            let _context_guard = WorkerContextGuard::enter(WorkerContext {
                scheduler_key: context.scheduler_key,
                inline_depth: context.inline_depth + 1,
            });
            wrapped();
            return Ok(JobHandle { receiver });
        }

        let mut state = lock_state(&self.shared);
        while state.external_admission_open && state.queue.len() >= self.shared.queue_capacity {
            state = wait_state(&self.shared.space_ready, state);
        }
        if !state.external_admission_open {
            return Err(SubmitError::Closed);
        }
        state.queue.push_back(wrapped);
        self.shared.work_ready.notify_one();
        Ok(JobHandle { receiver })
    }

    #[must_use]
    pub(crate) fn queue_len(&self) -> usize {
        lock_state(&self.shared).queue.len()
    }

    #[must_use]
    pub(crate) fn queue_capacity(&self) -> usize {
        self.shared.queue_capacity
    }

    #[must_use]
    pub(crate) fn external_admission_open(&self) -> bool {
        lock_state(&self.shared).external_admission_open
    }
}

/// Owns the fixed worker set for one task-run invocation.
///
/// The owner is deliberately `!Send`/`!Sync` via `Rc` phantom state. A worker
/// therefore cannot take ownership of the scheduler and attempt to join itself
/// during shutdown; only cloneable `SchedulerHandle`s cross thread boundaries.
pub(crate) struct BoundedScheduler {
    shared: Arc<SharedScheduler>,
    workers: Vec<JoinHandle<()>>,
    worker_count: usize,
    _owner_thread_only: PhantomData<Rc<()>>,
}

impl BoundedScheduler {
    pub(crate) fn new(config: SchedulerConfig) -> std::io::Result<Self> {
        let shared = Arc::new(SharedScheduler {
            state: Mutex::new(SchedulerState {
                queue: VecDeque::new(),
                external_admission_open: true,
            }),
            queue_capacity: config.queue_capacity(),
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
                    close_external_admission(&shared);
                    join_workers(&mut workers);
                    return Err(error);
                }
            }
        }

        Ok(Self {
            shared,
            workers,
            worker_count: config.worker_count(),
            _owner_thread_only: PhantomData,
        })
    }

    #[must_use]
    pub(crate) fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    #[must_use]
    pub(crate) const fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Stop admitting new external work, drain admitted jobs (including nested
    /// work required by those jobs), and join every worker.
    pub(crate) fn shutdown(mut self) -> Result<(), SchedulerShutdownError> {
        self.close_and_join()
    }

    fn close_and_join(&mut self) -> Result<(), SchedulerShutdownError> {
        close_external_admission(&self.shared);
        let panicked_workers = join_workers(&mut self.workers);
        if panicked_workers == 0 {
            return Ok(());
        }
        Err(SchedulerShutdownError { panicked_workers })
    }
}

impl Drop for BoundedScheduler {
    fn drop(&mut self) {
        let _shutdown_result = self.close_and_join();
    }
}

struct WorkerContextGuard {
    previous: WorkerContext,
}

impl WorkerContextGuard {
    fn enter(next: WorkerContext) -> Self {
        let previous = WORKER_CONTEXT.with(|current| current.replace(next));
        Self { previous }
    }
}

impl Drop for WorkerContextGuard {
    fn drop(&mut self) {
        WORKER_CONTEXT.with(|current| current.set(self.previous));
    }
}

fn scheduler_key(shared: &Arc<SharedScheduler>) -> usize {
    Arc::as_ptr(shared) as usize
}

fn lock_state(shared: &SharedScheduler) -> MutexGuard<'_, SchedulerState> {
    match shared.state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn wait_state<'a>(
    ready: &Condvar,
    state: MutexGuard<'a, SchedulerState>,
) -> MutexGuard<'a, SchedulerState> {
    match ready.wait(state) {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn close_external_admission(shared: &SharedScheduler) {
    let mut state = lock_state(shared);
    state.external_admission_open = false;
    shared.work_ready.notify_all();
    shared.space_ready.notify_all();
}

fn join_workers(workers: &mut Vec<JoinHandle<()>>) -> usize {
    let mut panicked_workers = 0usize;
    for worker in workers.drain(..) {
        if worker.join().is_err() {
            panicked_workers += 1;
        }
    }
    panicked_workers
}

fn worker_loop(shared: Arc<SharedScheduler>) {
    let _context_guard = WorkerContextGuard::enter(WorkerContext {
        scheduler_key: Some(scheduler_key(&shared)),
        inline_depth: 0,
    });
    worker_loop_inner(&shared);
}

fn worker_loop_inner(shared: &SharedScheduler) {
    loop {
        let next_job = {
            let mut state = lock_state(shared);
            loop {
                if let Some(job) = state.queue.pop_front() {
                    shared.space_ready.notify_one();
                    break Some(job);
                }
                if !state.external_admission_open {
                    break None;
                }
                state = wait_state(&shared.work_ready, state);
            }
        };

        match next_job {
            Some(job) => job(),
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    fn one_worker_one_slot() -> Result<SchedulerConfig, SchedulerConfigError> {
        SchedulerConfig::new(1, 1)
    }

    #[test]
    fn config_rejects_zero_and_extreme_resource_requests() -> TestResult {
        assert_eq!(
            SchedulerConfig::from_jobs(0),
            Err(SchedulerConfigError::ZeroWorkers)
        );
        assert_eq!(
            SchedulerConfig::from_jobs(MAX_WORKER_COUNT + 1),
            Err(SchedulerConfigError::TooManyWorkers {
                requested: MAX_WORKER_COUNT + 1,
                maximum: MAX_WORKER_COUNT,
            })
        );
        assert_eq!(
            SchedulerConfig::new(1, MAX_QUEUE_CAPACITY + 1),
            Err(SchedulerConfigError::QueueTooLarge {
                requested: MAX_QUEUE_CAPACITY + 1,
                maximum: MAX_QUEUE_CAPACITY,
            })
        );

        let config = SchedulerConfig::from_jobs(4)?;
        assert_eq!(config.worker_count(), 4);
        assert_eq!(config.queue_capacity(), 16);
        return Ok(());
    }

    #[test]
    fn live_job_concurrency_never_exceeds_worker_count() -> TestResult {
        let scheduler = BoundedScheduler::new(SchedulerConfig::from_jobs(2)?)?;
        let handle = scheduler.handle();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut jobs = Vec::new();

        for _ in 0..32 {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            jobs.push(handle.submit(move || {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                active.fetch_sub(1, Ordering::SeqCst);
            })?);
        }

        for job in jobs {
            job.join_timeout(Duration::from_secs(2))?;
        }

        assert!(peak.load(Ordering::SeqCst) <= scheduler.worker_count());
        scheduler.shutdown()?;
        return Ok(());
    }

    #[test]
    fn full_queue_applies_backpressure_and_never_spills() -> TestResult {
        let scheduler = BoundedScheduler::new(one_worker_one_slot()?)?;
        let handle = scheduler.handle();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let first_gate = Arc::clone(&gate);

        let first = handle.submit(move || {
            let _sent = started_tx.send(());
            let (lock, ready) = &*first_gate;
            let mut released = match lock.lock() {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            while !*released {
                released = match ready.wait(released) {
                    Ok(value) => value,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            1usize
        })?;

        started_rx.recv_timeout(Duration::from_secs(1))?;
        let second = handle.submit(|| 2usize)?;
        assert_eq!(handle.queue_len(), 1);
        assert_eq!(handle.queue_capacity(), 1);

        let third_handle = handle.clone();
        let (attempting_tx, attempting_rx) = mpsc::sync_channel(1);
        let (admitted_tx, admitted_rx) = mpsc::sync_channel(1);
        let submitter = thread::spawn(move || {
            let _attempting = attempting_tx.send(());
            let result = third_handle.submit(|| 3usize);
            let _admitted = admitted_tx.send(result);
        });

        attempting_rx.recv_timeout(Duration::from_secs(1))?;
        assert!(admitted_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert_eq!(handle.queue_len(), 1);

        let (lock, ready) = &*gate;
        match lock.lock() {
            Ok(mut released) => *released = true,
            Err(poisoned) => *poisoned.into_inner() = true,
        }
        ready.notify_all();

        let third = admitted_rx.recv_timeout(Duration::from_secs(1))??;
        assert!(submitter.join().is_ok());
        assert_eq!(first.join_timeout(Duration::from_secs(1))?, 1);
        assert_eq!(second.join_timeout(Duration::from_secs(1))?, 2);
        assert_eq!(third.join_timeout(Duration::from_secs(1))?, 3);
        scheduler.shutdown()?;
        return Ok(());
    }

    #[test]
    fn closing_external_admission_unblocks_waiting_producers() -> TestResult {
        let scheduler = BoundedScheduler::new(one_worker_one_slot()?)?;
        let handle = scheduler.handle();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let first_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = mpsc::sync_channel(1);

        let first = handle.submit(move || {
            let _sent = started_tx.send(());
            let (lock, ready) = &*first_gate;
            let mut released = match lock.lock() {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            while !*released {
                released = match ready.wait(released) {
                    Ok(value) => value,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
        })?;

        started_rx.recv_timeout(Duration::from_secs(1))?;
        let queued = handle.submit(|| 7usize)?;
        let blocked_handle = handle.clone();
        let (attempting_tx, attempting_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let submitter = thread::spawn(move || {
            let _attempting = attempting_tx.send(());
            let result = blocked_handle.submit(|| 9usize).map(|_| ());
            let _sent = result_tx.send(result);
        });

        attempting_rx.recv_timeout(Duration::from_secs(1))?;
        assert!(result_rx.recv_timeout(Duration::from_millis(50)).is_err());
        close_external_admission(&scheduler.shared);
        assert!(!handle.external_admission_open());
        assert_eq!(
            result_rx.recv_timeout(Duration::from_secs(1))?,
            Err(SubmitError::Closed)
        );

        let (lock, ready) = &*gate;
        match lock.lock() {
            Ok(mut released) => *released = true,
            Err(poisoned) => *poisoned.into_inner() = true,
        }
        ready.notify_all();
        assert!(submitter.join().is_ok());
        first.join_timeout(Duration::from_secs(1))?;
        assert_eq!(queued.join_timeout(Duration::from_secs(1))?, 7);
        scheduler.shutdown()?;
        return Ok(());
    }

    #[test]
    fn admitted_job_can_finish_nested_work_after_external_close() -> TestResult {
        let scheduler = BoundedScheduler::new(one_worker_one_slot()?)?;
        let handle = scheduler.handle();
        let nested_handle = handle.clone();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let parent_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = mpsc::sync_channel(1);

        let parent = handle.submit(move || {
            let _sent = started_tx.send(());
            let (lock, ready) = &*parent_gate;
            let mut released = match lock.lock() {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            while !*released {
                released = match ready.wait(released) {
                    Ok(value) => value,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            drop(released);

            let child = nested_handle.submit(|| 41usize)?;
            let value = child.join()?;
            Ok::<usize, Box<dyn std::error::Error + Send + Sync>>(value + 1)
        })?;

        started_rx.recv_timeout(Duration::from_secs(1))?;
        close_external_admission(&scheduler.shared);
        assert!(matches!(handle.submit(|| 1usize), Err(SubmitError::Closed)));

        let (lock, ready) = &*gate;
        match lock.lock() {
            Ok(mut released) => *released = true,
            Err(poisoned) => *poisoned.into_inner() = true,
        }
        ready.notify_all();

        let parent_result = parent.join_timeout(Duration::from_secs(1))?;
        assert_eq!(parent_result?, 42);
        scheduler.shutdown()?;
        return Ok(());
    }

    #[test]
    fn nested_same_scheduler_submission_completes_without_deadlock() -> TestResult {
        let scheduler = BoundedScheduler::new(one_worker_one_slot()?)?;
        let handle = scheduler.handle();
        let nested_handle = handle.clone();

        let parent = handle.submit(move || {
            let child = nested_handle.submit(|| 41usize)?;
            let value = child.join()?;
            Ok::<usize, Box<dyn std::error::Error + Send + Sync>>(value + 1)
        })?;

        let value = parent.join_timeout(Duration::from_secs(1))?;
        assert_eq!(value?, 42);
        scheduler.shutdown()?;
        return Ok(());
    }

    #[test]
    fn nested_inline_execution_has_a_stack_safety_limit() -> TestResult {
        fn submit_next(handle: SchedulerHandle, remaining: usize) -> Result<(), SubmitError> {
            if remaining == 0 {
                return Ok(());
            }
            let nested = handle.clone();
            let child = handle.submit(move || submit_next(nested, remaining - 1))?;
            match child.join() {
                Ok(result) => result,
                Err(_) => Err(SubmitError::NestedDepthExceeded {
                    maximum: MAX_INLINE_NESTING_DEPTH,
                }),
            }
        }

        let scheduler = BoundedScheduler::new(one_worker_one_slot()?)?;
        let handle = scheduler.handle();
        let nested = handle.clone();
        let parent = handle.submit(move || submit_next(nested, MAX_INLINE_NESTING_DEPTH + 1))?;
        let result = parent.join_timeout(Duration::from_secs(1))?;
        assert_eq!(
            result,
            Err(SubmitError::NestedDepthExceeded {
                maximum: MAX_INLINE_NESTING_DEPTH,
            })
        );
        scheduler.shutdown()?;
        return Ok(());
    }

    #[test]
    fn panicking_job_is_contained_and_later_work_still_runs() -> TestResult {
        let scheduler = BoundedScheduler::new(SchedulerConfig::from_jobs(1)?)?;
        let handle = scheduler.handle();

        let panicking = handle.submit(|| -> usize { panic!("expected test panic") })?;
        let following = handle.submit(|| 7usize)?;

        assert_eq!(
            panicking.join_timeout(Duration::from_secs(1)),
            Err(JobJoinError::Panicked)
        );
        assert_eq!(following.join_timeout(Duration::from_secs(1))?, 7);
        scheduler.shutdown()?;
        return Ok(());
    }

    #[test]
    fn explicit_shutdown_reports_internal_worker_panic() -> TestResult {
        let scheduler = BoundedScheduler::new(one_worker_one_slot()?)?;
        {
            let mut state = lock_state(&scheduler.shared);
            state
                .queue
                .push_back(Box::new(|| panic!("simulated internal worker failure")));
            scheduler.shared.work_ready.notify_one();
        }

        let result = scheduler.shutdown();
        let error = match result {
            Ok(()) => return Err("expected shutdown to report worker panic".into()),
            Err(error) => error,
        };
        assert_eq!(error.panicked_workers(), 1);
        return Ok(());
    }
}
