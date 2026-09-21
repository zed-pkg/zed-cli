#[allow(dead_code, unreachable_pub)]
#[path = "../src/task_scheduler.rs"]
mod task_scheduler;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use task_scheduler::{BoundedScheduler, JobJoinError, SchedulerConfig};

#[test]
fn config_from_jobs_is_bounded_and_rejects_zero() {
    assert!(SchedulerConfig::from_jobs(0).is_none());

    let config = SchedulerConfig::from_jobs(4).expect("non-zero jobs must produce config");
    assert_eq!(config.worker_count(), 4);
    assert_eq!(config.queue_capacity(), 16);
}

#[test]
fn live_job_concurrency_never_exceeds_worker_count() {
    let scheduler = BoundedScheduler::new(
        SchedulerConfig::from_jobs(2).expect("scheduler config must be valid"),
    )
    .expect("scheduler must start");
    let handle = scheduler.handle();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut jobs = Vec::new();

    for _ in 0..32 {
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        jobs.push(
            handle
                .submit(move || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .expect("job admission must succeed"),
        );
    }

    for job in jobs {
        job.join().expect("job must finish");
    }

    assert!(peak.load(Ordering::SeqCst) <= scheduler.worker_count());
    scheduler.shutdown();
}

#[test]
fn full_queue_applies_backpressure_instead_of_spilling() {
    let scheduler = BoundedScheduler::new(
        SchedulerConfig::new(
            std::num::NonZeroUsize::new(1).expect("one is non-zero"),
            std::num::NonZeroUsize::new(1).expect("one is non-zero"),
        ),
    )
    .expect("scheduler must start");
    let handle = scheduler.handle();

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_tx, started_rx) = mpsc::channel();
    let first_gate = Arc::clone(&gate);
    let first = handle
        .submit(move || {
            started_tx.send(()).expect("start signal must send");
            let (lock, ready) = &*first_gate;
            let mut released = lock.lock().expect("gate mutex poisoned");
            while !*released {
                released = ready.wait(released).expect("gate mutex poisoned");
            }
            1usize
        })
        .expect("first job admission must succeed");

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first job must start");

    let second = handle
        .submit(|| 2usize)
        .expect("second job should fill the single queue slot");
    assert_eq!(handle.queue_len(), 1);
    assert_eq!(handle.queue_capacity(), 1);

    let third_handle = handle.clone();
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let submitter = thread::spawn(move || {
        let third = third_handle
            .submit(|| 3usize)
            .expect("third job should eventually be admitted");
        admitted_tx.send(third).expect("admission signal must send");
    });

    assert!(
        admitted_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "third producer should block while queue is full"
    );

    let (lock, ready) = &*gate;
    *lock.lock().expect("gate mutex poisoned") = true;
    ready.notify_all();

    let third = admitted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("third job should be admitted after queue space opens");
    submitter.join().expect("submitter thread must not panic");

    assert_eq!(first.join().expect("first job must finish"), 1);
    assert_eq!(second.join().expect("second job must finish"), 2);
    assert_eq!(third.join().expect("third job must finish"), 3);
    scheduler.shutdown();
}

#[test]
fn nested_same_scheduler_submission_executes_without_deadlock() {
    let scheduler = BoundedScheduler::new(
        SchedulerConfig::new(
            std::num::NonZeroUsize::new(1).expect("one is non-zero"),
            std::num::NonZeroUsize::new(1).expect("one is non-zero"),
        ),
    )
    .expect("scheduler must start");
    let handle = scheduler.handle();
    let nested_handle = handle.clone();

    let parent = handle
        .submit(move || {
            let child = nested_handle
                .submit(|| 41usize)
                .expect("nested admission must succeed");
            child.join().expect("nested job must finish") + 1
        })
        .expect("parent admission must succeed");

    assert_eq!(parent.join().expect("parent job must finish"), 42);
    scheduler.shutdown();
}

#[test]
fn panicking_job_is_contained_and_later_work_still_runs() {
    let scheduler = BoundedScheduler::new(
        SchedulerConfig::from_jobs(1).expect("scheduler config must be valid"),
    )
    .expect("scheduler must start");
    let handle = scheduler.handle();

    let panicking = handle
        .submit(|| -> usize { panic!("expected test panic") })
        .expect("panicking job must still be admitted");
    let following = handle
        .submit(|| 7usize)
        .expect("following job must be admitted");

    assert_eq!(panicking.join(), Err(JobJoinError::Panicked));
    assert_eq!(following.join().expect("worker must survive panic"), 7);
    scheduler.shutdown();
}
