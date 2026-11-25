# Synchronous code

> **Status: prototype.** This chapter describes what works today. The
> [design](./design.md) describes the rest and the reasons behind it.

`#[psync::graph]` also works on an ordinary `fn`. There, the statements that overlap are the ones
that **block**: they wait on the network or a child process. psync runs independent blocking
statements on threads, and keeps results, errors and panics as the sequential code has them. The
analysis, the schedule and the [certificates](../safety/certificates.md) are the ones you know from
async code. Only the way a level runs changes: its chains become jobs on a pool of threads instead
of futures in one task.

It needs psync's `std` feature:

```toml
[dependencies]
psync = { version = "0.1", default-features = false, features = ["std"] }
```

## Which statements block

psync cannot see time, so it has to be told which statements are slow. It recognizes some by
path:

- the network: `ureq`, `reqwest::blocking`, `TcpStream`, `UdpSocket`;
- processes: `Command`.

Everything else needs a hint. Most slow calls in real code go through a helper (`download(url)`,
`repo.infos_index()`), and a helper's name says nothing. Mark such a statement with
`#[psync::par]`:

```rust,ignore
#[psync::graph]
fn three_downloads(log: &Mutex<Vec<&'static str>>) -> (u32, u32, u32) {
    #[psync::par]
    let a = slow(log, "a", 100, 1);
    #[psync::par]
    let b = slow(log, "b", 100, 2);
    #[psync::par]
    let c = slow(log, "c", 100, a + 10);
    (a, b, c)
}
```

```text
  region 0:
    n0: a = slow (log , "a" , 100 ,… [blocks]
    n1: b = slow (log , "b" , 100 ,… [blocks]
    n2: c = slow (log , "c" , 100 ,… [blocks]
    n0 -> n2 (reads `a`)
    level 0: n0->n2 || n1
  unclassified (assumed to read; `strict` treats them as writes): slow
```

`[blocks]` is the synchronous counterpart of `[await]`. `slow` is unclassified: psync cannot tell
from its name what it does to the world, so it assumes a read. See the next section. The schedule is the one async code would
get: `c` waits for `a`, and `b` runs beside both. The function takes two steps instead of three.

Two kinds of statement are deliberately *not* recognized as slow:

- **Ordinary method names.** In async code, an awaited `get` is a request. In synchronous code,
  `map.get(k)` is a lookup in memory, so a name never makes a statement blocking. (Names still
  decide *effects*, below.)
- **Files.** `std::fs` calls are recognized as file effects, so they keep their order with
  conflicting ones, but they are not treated as slow. A cached `stat` takes about 2 µs, and handing
  it to a thread costs more than that (see the measurements below). Mark a file statement with
  `#[psync::par]` when you know it is slow, for example on a network file system.

## What a blocking statement does to the world

Effects are found as in async code, with one change: a call does not have to be awaited to be
classified by its verb. In synchronous code, every method chain and free function named by a
verb is a service call. `client.delete(name)` writes `client`, and `client.get(name)` reads it, so
the two keep their order:

```text
  region 0 (unchanged):
    n0: # [psync :: par] client ? [blocks] {write `client` `name` (delete)}
    n1: v = client . get (name) ?? [blocks] {read `client` `name` (get)}
    n0 -> n1 (write `client` `name` (delete) vs read `client` `name` (get))
```

This only ever adds conflicts. A call without a known verb (`len`, `Vec::new`) has no effect and
is not reported, because most calls in synchronous code are computations. A blocking statement
whose calls all lack a verb is reported as **unclassified**, as an unclassified awaited call is,
and under `strict` it is a write to anything:

```text
  unclassified (assumed to read; `strict` treats them as writes): slow
```

A receiver bound *inside* a statement belongs to that statement. Two downloads that each open
their own socket `s` do not conflict, although both call `s.write_all(..)`.
[Different receivers are different services](../safety/effects.md) is the assumption psync makes
everywhere, and these are different receivers that happen to share a name. Two identical
statements still conflict.

## What stays as it was

**Errors.** Every chain records its `?`, `return`, `break` and `continue` instead of performing
them, and they are replayed in program order, exactly as in [Early exits](../guide/exits.md). If a
later statement fails first in time, the earlier one's error still wins:

```rust,ignore
#[psync::par]
let a: u32 = slow(log, "a", 100, Err("first"))?;
#[psync::par]
let b: u32 = slow(log, "b", 10, Err("second"))?;   // fails first, loses
```

**Panics.** A panic on a thread is caught. psync records which statement of the chain raised it,
and replays the panic at that statement's place in program order, with its original payload. An
earlier `Err` therefore still wins over a later panic, as it would in sequential code, and a panic
that does propagate is the one that was raised, not a generic "a scoped thread panicked".

**Writes after a possible exit.** A write is not performed early if an earlier statement may
leave the function, because sequential code would never have performed it. This is the rule that
decides tauri's case below. `#[psync::speculate]` lifts it for one statement.

**Locks.** A `let` that keeps a guard (`.lock()`, `.borrow_mut()`, `.read()` or `.write()` without
arguments) stays where it is, and splits the block, like an awaited lock in async code.

## What threads add: `Send` and `Sync`

A chain that runs on another thread may only borrow what can be shared between threads. If it
cannot, rustc says so, at the attribute:

```rust,ignore
#[psync::graph]
fn not_sync(cell: &std::cell::Cell<u32>, log: &Mutex<Vec<&'static str>>) -> u32 {
    #[psync::par]
    let a = slow(log, "a", 1, cell.get());
    #[psync::par]
    let b = slow(log, "b", 1, 2);
    a + b
}
```

```text
error[E0277]: `Cell<u32>` cannot be shared between threads safely
   --> tests/sync.rs:127:1
    |
127 | #[psync::graph]
    | ^^^^^^^^^^^^^^^ `Cell<u32>` cannot be shared between threads safely
    |
    = note: required for `&Cell<u32>` to implement `Send`
```

As with two `&mut` borrows in async code, this is a compile error, not a race. Mark the statement
`#[psync::seq]`, or share the value in a form that is `Sync`.

One hazard has no type, and psync cannot see it yet: **thread-local state**. A callee that keeps a
cache, a connection or a `tracing` span in a `thread_local!` sees a different one on a pool
thread. The [design](./design.md#2-threads-are-not-tasks-three-new-hazards) plans a *thread*
effect, which the driver can find in compiled code. Until then, do not mark such calls
`#[psync::par]`.

## How the jobs run

A level of two or more chains becomes one call to `psync::__private::par_run`. The first chain
runs on the calling thread, and the others on a pool of parked worker threads, and the call
returns when all have finished. Because it waits, the jobs may borrow from the function's stack,
as with `std::thread::scope`. Waking a parked worker costs about a tenth of what creating a thread
does (measured in the [design](./design.md#1-which-statements-are-worth-a-thread)). The pool grows
when every worker is busy, so a job that blocks never waits for a worker that is itself waiting.
Past 256 workers, a job runs on the calling thread. That changes when it runs, never what it
computes.

The pool is psync's own, not rayon's. Its jobs block on I/O, and blocking rayon's CPU-sized pool
would stall every other user of rayon in the process.

## Measurements

`examples/phase2.rs` rebuilds three of the audit's synchronous findings over real sockets and
files, written the way the projects write them. tauri calls a `download(url)` helper, and rustic
calls methods on a repository (`repo.list_files()`, `repo.read_index()`). A local server delays
every response by 50 ms, standing in for a remote backend. Each figure is the median of 21 runs
(2,001 for the microsecond rows), on a 32-thread machine that other jobs were also using (load
average about 50):

| Shape | Sequential | psync | Speedup |
|---|---|---|---|
| tauri `prepare_tools`: three downloads, then three writes | 152.1 ms | 51.0 ms | 2.98× |
| rustic `repoinfo`: list the repository, read its index | 101.1 ms | 50.5 ms | 2.00× |
| tauri `prepare_tools` **as written**: three `if !exists { download; write }` | 152.0 ms | 152.1 ms | 1.00× |
| the same, later `if`s marked `#[psync::speculate]` | 152.3 ms | 51.0 ms | 2.99× |
| two cached `stat`s marked `#[psync::par]` (a miss) | 3.6 µs | 13.5 µs | 0.26× |
| `par_run` with two empty jobs | | 12–21 µs | |

The machine's load shows in the parallel versions, which need six threads to run at once (three
jobs, and the server's three). Across eight runs, the first row's psync median ranged from 51 ms
to 138 ms (1.1× to 3.0×), while the sequential medians stayed at 152 ms. Most runs gave the
figures above. One early run also measured 488 ms for a sequential median, which no later run
repeated.

The rows that do not speed up are the interesting ones:

- **tauri as written: 1.00×, and correct.** Each `if` downloads a tool and writes it. If the first
  download fails, the function returns, and sequential code never writes the second file. psync
  keeps that: `n0 -> n1 (may exit before fs::write runs)`. A complete file in a tool cache is
  harmless, so here the right answer is to say so with `#[psync::speculate]`, which gives 2.99×.
  Building these rows also turned up two false edges and one missing one, all now fixed:
  - The targets `format!("{dir}/AppRun")` and `format!("{dir}/linuxdeploy")` used to conflict,
    because psync kept only the text before the first `{`. They are now compared piece by piece
    ([When two effects conflict](../safety/effects.md#when-two-effects-conflict)), in async code
    too.
  - `let data: u32 = ..` inside a block was read as an assignment to `u32`, in async code too.
  - The first prototype gave synchronous method calls no effects, so a `#[psync::par]` delete
    and get of one name would have overlapped. Verbs now classify synchronous calls (above).
- **Cached `stat`s: 0.18×.** A thread costs more than a cached `stat`. That is why file statements
  are not treated as slow by default, and why the design calls for an adaptive loop that times the
  first item before handing out the rest.

## Not yet

- **Loops.** `for` and `while` in synchronous code are left as written. No prefetching or fission
  yet.
- **Compiler facts.** `cargo psync build` records what each callee does, but the macro does not
  yet use those facts to decide which statements block, so helpers need `#[psync::par]`.
- **`cargo psync check`** does not check synchronous levels yet. They carry none of the markers it
  reads.
- **The *thread* effect**, described above.
- **The proof's last step.** The theorems are about whole statements, and threads interleave
  finer. The [soundness argument](./design.md#soundness-argument) explains why data-race freedom
  closes the gap. The Lean statement of it is still to be written.

## Exercises

1. In `three_downloads`, make `b` use `a` too. What does the explanation print, and how long does
   the function take now?
2. Remove `#[psync::speculate]` from one of the two later `if`s in `prepare_tools_speculate`
   (`examples/phase2.rs`). Which edge comes back, and what does the measurement become?
3. Why is a `let guard = m.lock().unwrap();` a barrier, while
   `*m.lock().unwrap() += 1;` may move? (Hint: when is the guard dropped?)
