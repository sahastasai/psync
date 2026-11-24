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
  it.
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

> The script builds an `rp2350` example. That example is not on the `claude/initial_psync_graph`
> branch, which removed the missing entry from `Cargo.toml`. Until it is restored, the firmware
> build step fails, and the host tests still run.

## With `#[psync::graph]`

The generated code joins futures by polling them in one task. On psync's executor, an annotated
function's concurrent branches therefore share their task's priority. Spawn separate tasks when
different pieces of work need different priorities.
