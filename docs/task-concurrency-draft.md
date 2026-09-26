# Bounded task concurrency design

Tracking: https://github.com/zed-pkg/zed-cli/issues/472

This draft defines the resource and scheduling invariants for replacing per-group scoped thread creation with a fixed worker pool and bounded in-memory work queue.

## Required invariants

- The number of live worker threads is bounded by the configured worker count.
- The number of concurrently running child processes is bounded independently from worker count.
- The runnable-job queue has a finite capacity derived from the worker count.
- Queue saturation applies backpressure; graph size must not translate into unbounded memory growth.
- Task identity deduplication remains race-safe and preserves the existing `Mutex + Condvar` execution-state contract.
- Nested task execution cannot deadlock by filling or waiting on the same worker pool.
- Worker panics/errors are contained and surfaced deterministically.
- Shutdown stops admission, drains already-admitted work according to documented semantics, and joins all workers before `TaskRuntime::run` returns.

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

The nested-work rule is important. A naive bounded pool can deadlock when all workers synchronously enqueue child jobs and then wait for those children. The implementation should either execute same-scheduler nested work inline, have waiting workers help drain runnable work, or schedule the entire dependency graph from a coordinator so workers never synchronously wait on child work. The first draft should choose one behavior explicitly and test it.

## Separate subprocess budget

The existing `CommandLimiter` should remain an independent permit layer. A worker thread is cheap relative to a compiler, linker, package manager, or arbitrary shell child process; coupling both limits unnecessarily makes future CPU-bound and I/O-bound scheduling harder.

The initial compatibility contract remains:

- `--jobs N` must be positive;
- the current default remains `1` unless a separate change intentionally revises it;
- `ZED_TASK_JOBS` and CLI validation remain synchronized;
- no new async runtime dependency is required just to bound local work.

## Queue capacity

The queue must be bounded. A reasonable first policy is a small multiple of worker count, calculated with saturating arithmetic, for example `max(1, jobs.saturating_mul(4))`. This value should be centralized rather than duplicated across call sites.

Changing queue capacity should not alter task semantics; it only changes how much runnable work can wait in memory before producers apply backpressure.

## Migration sequence

1. Add the scheduler primitive and focused unit tests for thread bounds, queue bounds, nested submission, panic containment, and shutdown.
2. Route parallel task groups through the scheduler while preserving the existing execution-state deduplication logic.
3. Keep child process execution behind `CommandLimiter` and add peak-concurrency tests.
4. Remove per-chunk `thread::scope(...scope.spawn...)` fan-out only after nested task tests prove the replacement cannot deadlock.
5. Add repository policy/static checks that reject new task-path ad hoc spawning outside the scheduler.
6. Stress a large synthetic task graph and assert that worker count, queue occupancy, and process concurrency remain within configured bounds.

## Draft acceptance tests

The implementation PR should eventually contain tests equivalent to:

- 10,000 runnable nodes with `jobs = 4` never create more than four scheduler workers;
- queue occupancy never exceeds configured capacity;
- when the queue is full, producers do not allocate an unbounded spill queue;
- a worker can invoke a nested task that itself invokes another task without deadlocking;
- duplicate task identities still execute exactly once;
- a panicking/failed job does not silently kill a worker and strand queued work;
- scheduler shutdown joins every worker;
- child process peak concurrency never exceeds the command permit budget.

## Explicit non-goals for the preliminary draft

This document does not claim the runtime has already migrated to a fixed pool. Until the scheduler implementation and nested-task tests land, the existing bounded scoped-thread behavior remains authoritative. This draft exists to make the replacement invariants reviewable before changing execution semantics.
