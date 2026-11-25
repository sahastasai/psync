# The executor

psync's executor is a small, `no_std`, heap-backed, cooperative async executor. It was written for the
RP2350's ARM cores, and it runs on any target with `alloc`. `#[psync::graph]` does not need it. It is
here when you want priorities on a microcontroller.

## The model

```rust,ignore
let executor = psync::Executor::default();
let handle = executor.spawn(10, async { /* work */ });
handle.set_priority(20);
executor.run_until_stalled(); // or executor.run() to park and wait forever
```

- **Priority only.** Each task has a `u16` priority. Larger values run first.
- **FIFO among equals.** The pool takes the first ready task of the highest priority. A task that is
  still pending goes to the back of the queue, so ready tasks of equal priority take turns.
- **Sleeping tasks are not polled.** A task is polled only after a wake. A wake that arrives during
  a poll is kept: the ready flag is cleared *before* polling.
- **No preemption.** A task that is always ready and has a higher priority starves the ones below
  it. `MultiExecutor` can bound that ([Fairness](#fairness)), and yield injection gives long
  loops preemption points ([Yield injection](#yield-injection)).
- **One core.** Keep each executor on one core. `Executor` holds its pool in an `Rc`. Wakers and
  `TaskHandle`s are `Send + Sync`, so interrupts and other threads can wake tasks and change their
  priorities.

The scheduling tests (`tests/scheduling.rs`) pin these rules down. For example:

```rust,ignore
let executor = Executor::default();
for (priority, id) in [(1, 0), (8, 1), (8, 2), (3, 3)] {
    let log = log.clone();
    executor.spawn(priority, async move { log.borrow_mut().push(id) });
}
executor.run_until_stalled();
assert_eq!(*log.borrow(), [1, 2, 3, 0]);   // 8, 8 (in spawn order), 3, 1
```

## API

| Item | Purpose |
|---|---|
| `Executor::default()` / `Executor::new(pool)` | an executor with the default pool, or with your own `Pool` |
| `spawn(priority, future)` | schedule a `'static` future with `Output = ()`; returns a `TaskHandle` |
| `spawn_boxed(future)`, `spawn_future(boxed)`, `spawn_future_with_priority(p, boxed)` | the same, at priority 0 or with a pre-boxed future |
| `poll_once()` | poll one ready task; `false` when every remaining task is sleeping |
| `run_until_stalled()` | poll until no task is ready |
| `run() -> !` | run forever, parking when nothing is ready |
| `is_empty()` | no tasks left |
| `TaskHandle::priority()`, `set_priority(p)`, `wake()` | inspect or change a task from anywhere; `set_priority` also wakes it |

`BoxFuture` and `LocalBoxFuture` are the boxed future types the executor stores.

## Pools and parking

Two traits make the executor portable:

```rust,ignore
/// A local task queue. `take_ready` must remove the highest-priority ready task.
pub trait Pool {
    type Park: Park;
    fn push(&self, task: Task);
    fn take_ready(&self) -> Option<Task>;
    fn is_empty(&self) -> bool;
}

/// Parking must preserve an unpark notification delivered just before park.
pub trait Park: Default + Send + Sync + 'static {
    fn park(&self);
    fn unpark(&self);
}
```

`DefaultPool` is a `VecDeque` scanned for the highest-priority ready task. `DefaultPark` uses
`WFE`/`SEV` on the RP2350 and spins elsewhere. To sleep properly on a host, plug in your own `Park`,
for example one built on a condition variable.

## On the RP2350

With the default `rp235x` feature on `thumbv8m.main-none-eabihf`, psync provides the global
allocator. Initialize it once before the first allocation:

```rust,ignore
unsafe { psync::init_heap() };
```

The `memory.x` in the repository assumes 4 MiB of flash (Pico 2). It places the heap after all
statics and reserves the top 32 KiB of RAM for the stack. Firmware also needs the RP2350 image
definition and cortex-m-rt's linker script (`build.rs` passes `-Tlink.x` to examples on that target).

```sh
rustup target add thumbv8m.main-none-eabihf
./scripts/test-rp2350.sh            # host tests, then a linked RP2350 build
./scripts/test-rp2350.sh --flash    # also download with probe-rs
```

> The script builds an `rp2350` example, which is not in the repository at the moment. Until it
> is restored, the firmware build step fails, and the host tests still run.

## With `#[psync::graph]`

The generated code joins futures by polling them in one task. On psync's executor, an annotated
function's concurrent branches therefore share their task's priority. Spawn separate tasks when
different pieces of work need different priorities.

## Many cores: `MultiExecutor`

With the `std` feature, `MultiExecutor` runs `Send` futures on worker threads that share one ready
queue:

```rust,ignore
let executor = psync::MultiExecutor::new(4);               // four workers
let ui = executor.spawn(20, async { /* latency-sensitive */ });
executor.spawn(1, async { /* batch work */ });
ui.set_priority(30);                                        // moves it in the queue right away
let answer = executor.block_on(5, async { 6 * 7 });          // run and wait for an output
executor.wait_idle();                                       // until every task has finished
```

The rules are the single-core executor's, across threads:

- A worker always takes the ready task with the highest priority. Tasks of equal priority run in
  the order they became ready.
- Changing the priority of a queued task re-keys it at once, keeping its age among equals.
- A wake that arrives while the task is being polled is never lost. The task goes idle, queued,
  running, or running-and-notified, and in the last state it is re-queued when the poll returns.

`tests/multi.rs` checks priority order, live priority changes, fairness, `block_on`, and that four
100 ms CPU-bound tasks on four workers finish in well under the 400 ms they take one after another.

## Fairness

Strict priorities starve. A high-priority task that is always ready keeps every lower one waiting.
`MultiExecutor::with_fairness(workers, n)` makes every `n`-th pick take the task that has been
ready the longest, whatever its priority:

| Executor | When a low-priority task runs, next to a busy high-priority task that yields 200 times |
|---|---|
| `MultiExecutor::new(1)` | after all 200 steps |
| `MultiExecutor::with_fairness(1, 4)` | within the first 4 steps |

A ready task then waits at most about `n` times the number of tasks ahead of it in age order. The
cost is that 1 in `n` picks may run lower-priority work ahead of higher-priority work.

## Yield injection

A cooperative executor cannot interrupt a task that does not await. A long loop of CPU work holds
its worker until it ends, even when a higher-priority task has become ready.
`#[psync::graph(yield_every = N)]` inserts a preemption point at the start of every loop body in
the async code it rewrites:

```rust,ignore
#[psync::graph(yield_every = 64)]
async fn index(docs: &[Doc]) -> Index {
    let mut index = Index::default();
    for doc in docs {          // every 64th iteration: psync::yield_now().await
        index.add(doc);        // CPU work, no awaits
    }
    index
}
```

`psync::yield_now()` returns `Pending` once and wakes its own task. That works on any executor:
psync's, tokio's, or another. The yields change timing only, never results. In
`tests/graph.rs`, a loop of six steps wakes an urgent task in step 2. With `yield_every = 2`, the
urgent task runs before step 4 instead of after step 6.

## Still open

- **Dual-core RP2350.** `MultiExecutor` uses `std` threads. Running it on the RP2350's second core
  needs the HAL's multicore launch and a spinlock-protected queue, and has to be tested on a board.
- **Parallel async branches.** The code `#[psync::graph]` generates for async functions joins its
  branches in one task. After the I/O overlaps, CPU work in the branches runs on one core (see
  [Patterns and pitfalls](./patterns.md)). Spawning them onto workers needs `'static` futures or
  scoped tasks, and the runtime's spawner. For synchronous functions, Phase II already runs
  independent statements as jobs on threads ([Synchronous code](../phase2/using.md)).

## Exercises

The answers to 1 and 2 are tests in `tests/scheduling.rs` (`book_*`).

1. Three tasks, spawned in this order: `a` at priority 2, `b` at 2, `c` at 7. Each logs its name
   when polled, wakes itself, and returns `Pending` the first time and `Ready` the second. What
   is the log after `run_until_stalled()`?
2. Three tasks that log once: `a` at 1, `b` at 2, `c` at 3. Before running the executor, you call
   `set_priority(10)` on `a`'s handle. What is the log?
3. A task at priority 3 runs an annotated function with two concurrent branches. Another task at
   priority 5 is ready the whole time. When do the branches run?
4. On `MultiExecutor::with_fairness(1, 4)`, a priority-9 task is always ready, and a priority-1
   task becomes ready. Roughly when does it run, and what does that cost?

<details>
<summary>Answers</summary>

1. `c c a b a b`. `c` is the only task at 7, so it runs again as soon as it wakes itself. Then
   `a` and `b` take turns: a task that is still pending goes to the back of the queue, behind
   its equals.
2. `a c b`. The priority that counts is the one when the task is taken from the queue.
3. Not while the priority-5 task stays ready. The branches are joined inside the priority-3
   task, so they share its priority, and the executor always takes the highest-priority ready
   task. They run once the priority-5 task finishes or sleeps (or, on a fair `MultiExecutor`, on
   its fairness picks). To give a branch its own priority, spawn it as a task.
4. Within about 4 picks: every 4th pick takes the task that has been ready longest. The cost is
   that the priority-9 task waits during that pick.

</details>
