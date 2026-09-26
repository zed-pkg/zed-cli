# Bounded task concurrency design

Tracking: https://github.com/zed-pkg/zed-cli/issues/472

This draft defines the resource and scheduling invariants for replacing per-group scoped thread creation with a fixed worker pool and bounded in-memory work queue.

## Required invariants

- The number of live worker threads is bounded by the configured worker count and an absolute safety ceiling.
- The number of concurrently running child processes is bounded independently from worker count.
- The runnable-job queue has a finite capacity derived from the worker count and an absolute safety ceiling.
- The number of admitted-but-unreaped jobs/results is bounded; callers must not retain O(graph_size) `JobHandle`s while workers continue draining the queue.
- Queue saturation applies backpressure; graph size must not translate into unbounded memory growth in either queued jobs or completion/result state.
- Task identity deduplication remains race-safe and preserves the existing `Mutex + Condvar` execution-state contract.
- Nested task execution cannot deadlock by filling or waiting on the same worker pool.
- Inline nested execution cannot recurse without a finite depth bound.
- Worker panics/errors are contained and surfaced deterministically.
- Mutex poisoning does not automatically turn an otherwise recoverable scheduler state into a second panic path.
- Shutdown closes new external admission while allowing already-admitted work to complete required nested work, then joins all workers.
- The scheduler owner cannot be moved into one of its own workers and attempt to join itself.

## Proposed first implementation

Use a small std-only scheduler owned by one `TaskRuntime::run` invocation:

```text
producer/coordinator
       |
       v
+----------------------+       +----------------------+
| bounded VecDeque<Job>| ----> | fixed worker threads |
| Mutex + Condvar      |       | N = configured jobs  |
+----------------------+       +----------------------+
       |
       +-- full => explicit backpressure

worker -> nested same-scheduler task
       => execute/help inline rather than synchronously enqueue-and-wait
```

The nested-work rule is important. A naive bounded pool can deadlock when all workers synchronously enqueue child jobs and then wait for those children. The current draft executes same-scheduler nested work inline and caps inline nesting depth. A later implementation may instead use worker-helping or a coordinator-driven DAG if that gives cleaner cancellation/fairness semantics.

## Hard resource ceilings

A positive integer alone is not a sufficient safety contract. `--jobs 18446744073709551615` must not become an attempt to reserve an enormous queue or spawn an enormous number of native threads.

The draft scheduler therefore rejects values above an explicit absolute worker ceiling and rejects queue capacities above an explicit queue ceiling. It also avoids eagerly allocating the full queue capacity: `VecDeque::new()` grows only as bounded work is actually admitted.

The preliminary constants are implementation details pending review; the key requirement is that the ceiling exists, is validated before worker creation, and is surfaced as an error rather than silently clamped.

## Bounded admission and completion state

A bounded runnable queue does **not** by itself prove bounded scheduler memory.

For example, a coordinator could continuously submit new work whenever one queue slot opens, keep every returned `JobHandle`, and never reap completed results until a 100,000-node graph has been fully admitted. Worker count and queue length would remain bounded while result receivers and other caller-owned completion state grow with graph size.

`TaskRuntime` integration therefore needs an explicit admission window. The coordinator must cap the number of externally admitted jobs whose results have not yet been reaped. A straightforward compatibility rule is:

```text
max_unreaped_external_jobs <= worker_count + queue_capacity
```

The exact formula can change after measurement, but it must be finite, derived from validated scheduler limits, and independent of dependency-graph size. When the admission window is full, the coordinator reaps at least one result before submitting more work.

This is a coordinator/runtime invariant rather than something the low-level `JobHandle` type can enforce after ownership has been handed to an arbitrary caller. For that reason the preliminary scheduler primitive is not considered sufficient for production `TaskRuntime` integration until the bounded admission/reaping layer exists and is stress-tested.

An alternative future API is a scheduler-owned completion stream or bounded batch/scope API that makes unreaped completion count part of the type-level execution surface. That may be preferable if the coordinator otherwise has too much freedom to retain handles.

## Scheduler ownership and self-join prevention

The object that owns worker `JoinHandle`s must stay on the coordinator thread. Only lightweight submission handles should cross worker boundaries.

The draft makes `BoundedScheduler` intentionally `!Send`/`!Sync` by construction while `SchedulerHandle` remains cloneable and thread-safe. This excludes the self-join failure mode where a worker takes ownership of the pool and calls shutdown on itself.

## Shutdown semantics

Shutdown has two admission classes:

1. **new external submissions** — rejected as soon as shutdown begins;
2. **nested work required by already-admitted jobs** — still allowed inline so draining can make forward progress.

Workers drain jobs already in the bounded queue, including nested work required by those jobs, and are joined before shutdown returns. External producers blocked on a full queue are awakened and receive a closed-admission error rather than remaining blocked forever.

Dropping the scheduler uses the same close-and-join path. Explicit shutdown additionally reports worker-thread panics discovered during join.

## Panic, poison, and timeout behavior

User job panics are caught at the job boundary and returned through the job handle so one user failure does not silently kill a worker and strand queued work.

Scheduler mutex acquisition recovers the guarded state from poisoning rather than calling `expect`/`unwrap` and creating a second panic path. An internal worker panic is still treated as a scheduler failure and surfaced by explicit shutdown.

A timed job-handle wait is not cancellation. Timing out drops that result receiver while the already-admitted job remains owned by the scheduler and may still finish. Cancellation semantics should be designed separately rather than being implied by a wait timeout.

## Separate subprocess budget

The existing `CommandLimiter` should remain an independent permit layer. A worker thread is cheap relative to a compiler, linker, package manager, or arbitrary shell child process; coupling both limits unnecessarily makes future CPU-bound and I/O-bound scheduling harder.

The initial compatibility contract remains:

- `--jobs N` must be positive;
- the current default remains `1` unless a separate change intentionally revises it;
- `ZED_TASK_JOBS` and CLI validation remain synchronized;
- an explicit value above the scheduler safety ceiling must be rejected, not silently capped;
- no new async runtime dependency is required just to bound local work.

## Queue capacity

The queue must be bounded. The current draft derives queue capacity as a small multiple of worker count, calculated centrally and validated against an absolute ceiling.

Changing queue capacity should not alter task semantics; it only changes how much runnable work can wait in memory before producers apply backpressure.

Queue capacity and completion-window capacity are separate limits: queue capacity bounds work waiting to run; the completion window bounds admitted jobs/results retained by the coordinator.

## Migration sequence

1. Add the scheduler primitive and focused unit tests for worker bounds, queue bounds, extreme input validation, nested submission, nested-depth safety, panic containment, shutdown, and blocked-producer wakeup.
2. Compile the scheduler through the normal library module graph; do not rely on a test-only `#[path = ...]` copy of the source.
3. Add a bounded coordinator admission/reaping window so `JobHandle`/completion state cannot grow with graph size.
4. Route parallel task groups through the scheduler while preserving the existing execution-state deduplication logic.
5. Keep child process execution behind `CommandLimiter` and add peak-concurrency tests.
6. Remove per-chunk `thread::scope(...scope.spawn...)` fan-out only after nested task tests prove the replacement cannot deadlock.
7. Add repository policy/static checks that flag new task-path ad hoc spawning outside the scheduler.
8. Stress a large synthetic task graph and assert that worker count, queue occupancy, unreaped completion count, and process concurrency remain within configured bounds.

## Draft acceptance tests

The implementation PR should eventually contain tests equivalent to:

- 10,000 runnable nodes with `jobs = 4` never create more than four scheduler workers;
- the same 10,000-node run never retains more than the configured admission-window number of unreaped job/result handles;
- worker and queue requests above their absolute ceilings fail before allocation/spawn;
- queue occupancy never exceeds configured capacity;
- when the queue is full, producers do not allocate an unbounded spill queue;
- a producer blocked on a full queue is released with `Closed` when external admission closes;
- a worker can invoke nested work after shutdown has closed external admission, allowing an already-admitted parent to finish;
- recursive inline nested work is stopped by a finite depth guard before stack growth becomes unbounded;
- duplicate task identities still execute exactly once after runtime integration;
- a panicking user job does not silently kill a worker and strand queued work;
- an internal worker panic is observable at explicit shutdown;
- scheduler shutdown joins every worker;
- child process peak concurrency never exceeds the command permit budget.

## Explicit non-goals for the preliminary draft

This document does not claim the runtime has already migrated to a fixed pool. Until the scheduler implementation, bounded coordinator admission/reaping, and nested-task tests land and pass normal repository validation, the existing bounded scoped-thread behavior remains authoritative. This draft exists to make the replacement invariants reviewable before changing execution semantics.
