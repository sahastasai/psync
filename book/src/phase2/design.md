# Phase II design: concurrency in synchronous code

> **Status: design.** Nothing in this chapter is implemented yet. It is the plan for the
> prototype (#159) and the audit (#166), and it records the measurements and decisions behind it.

Phase I makes independent *awaits* overlap. A lot of Rust waits without awaiting: build tools and
CLIs call `std::fs`, run `Command::output`, and download with `ureq` or `reqwest::blocking`, one
call after another. Phase II applies the same analysis, the same schedule, the same proofs and the
same certificates to synchronous code, and runs the chains on threads.

## What the audit found

The [audit](../introduction.md#where-psync-is-going) has ten findings of this kind (pattern P6).
They come in three shapes:

| Shape | Example (finding) | Today | With Phase II |
|---|---|---|---|
| independent blocking calls | rustic `repoinfo` (tools-29): `files: repo.infos_files()?`, `index: repo.open(..)?.infos_index()?` | t_list + t_index | max of the two, up to 2× on S3 or rclone backends |
| guarded downloads | tauri `prepare_tools` (app-16): three `if !path.exists() { download(..)?; write(..)?; }` | three sequential `ureq` GETs | up to about 3× on a cold cache |
| a loop over independent items | rustic `backup` (tools-31): one snapshot per source set | the sum over sets | about the maximum, if the sources are on different disks or the backend is latency-bound |

A fourth group is **blocking calls inside async code**: poem's directory listing (web-41), pixi's
input and output walks (tools-17), and neon's endpoint stops (dbc-11). These block an executor
thread, and they are independent of each other. They need the same analysis with a different
runtime: move the call off the executor and await it.

Two things stand out. The expensive calls are opaque by name: `download`, `infos_files`,
`backup_snapshot`, `node_from_snapshot_and_path`. And several gains depend on conditions
(remote backend, cold cache, different disks), so Phase II needs a cost model, not just a
dependence graph.

## What carries over unchanged

- **The analysis.** Nodes, edges, barriers, chains, levels, loop prefixes and fission work on any
  block. Their inputs are footprints and effects, not awaits.
- **Exits.** `chain_code` already turns `?`, `return`, `break` and `continue` into recorded exits,
  and the replay is the same, including the release rule of [Early exits](../guide/exits.md).
- **The proofs and certificates.** `check_sound` holds for any meaning of the statements under
  which the certificate's independence is real independence (see the soundness argument below).
- **The driver.** `cargo psync check` reads the same markers, and compiler facts refine the same
  footprints.

## What is new

### 1. Which statements are worth a thread

In async code, `.await` marks where the time goes. In synchronous code nothing does, and a
thread costs real time. Measured on the development machine (median of 2,000 runs, release
build):

| Mechanism | Cost |
|---|---|
| `std::thread::scope` with 1 spawned thread | 126 µs |
| with 2 / 4 / 8 spawned threads | 150 / 263 / 451 µs |
| hand a job to a parked pool thread and wait for it | 10 µs |

A fresh thread per chain pays for itself only above about a quarter of a millisecond. A pool of
parked workers brings that down to tens of microseconds. So psync runs chains on a **persistent
pool with a scoped API**: borrowing jobs, joined before the level ends, like `rayon::scope`. It
does not use rayon itself, because rayon's pool is sized for CPU work, and blocking I/O on it
starves everything else that uses rayon.

Statements fall into three cost classes:

| Class | Recognized by | Default |
|---|---|---|
| **remote**: network, processes | path (`Command::output`, `TcpStream`, `ureq`/`reqwest::blocking` calls), or a compiler fact that the callee's summary reaches the network or a process | overlap, whenever two are independent |
| **local I/O**: files | `std::fs` paths, or a fact that the callee reaches the file system | overlap in loops, adaptively (below); pairs only with a hint |
| **CPU** | everything else | only with a hint, `#[psync::par]` |

The verb heuristics of Phase I are **not** used in synchronous code: `map.get(k)` is not a
request. That leaves names like `download` unrecognized without compiler facts, which is why
`cargo psync build` matters more in Phase II. The driver knows that `download`'s body calls
`ureq::Request::call`.

**Adaptive loops.** A cached `stat` takes 2.4 µs here (median, measured the same way), four
times less than handing it to a pool thread, while a `stat` on S3 or a cold network share takes
milliseconds. The same loop can be worth parallelizing on one machine and not on another. For a
loop of the local-I/O or CPU class, the generated code runs the first item's prefix on the calling
thread and times it. It hands the remaining items to the pool only if
`elapsed × remaining items > K × handoff cost`, with `K` about 10. The result is the same either
way. Only the timing changes.

### 2. Threads are not tasks: three new hazards

Moving a call to another thread is visible in ways that moving it to another point of the same
task is not.

1. **`Send` and `Sync`.** A chain that captures `&RefCell<..>`, `Rc<..>` or a `!Send` connection
   cannot run on another thread. rustc rejects the generated code, as it rejects two `&mut`
   borrows in Phase I. That is again *a compile error, not a race*. The fix is the same:
   `#[psync::seq]`.
2. **Thread-local state.** A callee that reads a `thread_local!` (a cache, an RNG, a `tracing`
   span, a SQLite handle stored per thread) behaves differently on a pool thread, and no type
   says so. The driver can see it: a summary that reaches `LocalKey::with` gets a new effect,
   *thread*, which conflicts with every other thread effect and makes the call stay on the
   calling thread. Without facts, the macro cannot see thread-locals. This is a documented
   assumption, and the reason the remote class is the only default.
3. **Panics.** `std::thread::scope` re-panics with a generic message, and a panicking chain would
   win over an earlier `?` that sequential code would have taken first. Each job instead runs
   under `catch_unwind`, and a panic is recorded like any other exit and replayed in program
   order with `resume_unwind`. The original payload survives, and an earlier `Err` still wins.

### 3. Output and ordering

`println!`, logging and progress bars are ordered writes to standard output. psync keeps them in
order relative to each other, as it does in async code. Findings such as rustic's `copy` print
tables from inside the parallel part. There, the output stays in order only if each table is
written by its own statement, after the join.

## Code generation

A level of two or more chains becomes a scoped join on the pool. Each chain body is exactly
`chain_code`'s body without the `async` wrapper:

```rust,ignore
let (__psync_taken0, __psync_taken1) = ::psync::__private::par_join2(
    || { /* chain 0: statements, exits recorded, returns (slots.., Option<bindings>) */ },
    || { /* chain 1 */ },
);
// unpack, replay exits and panics in program order, bind: as in Phase I
```

The calling thread runs chain 0 and the pool runs the rest. `par_join2` is a function, not a
spawn, so the closures may borrow from the stack frame, and the borrow checker checks them as it
checks any closures. A loop prefix becomes `par_map_bounded(items, bound, |item| prefix)`, with
results in item order and the adaptive first item.

## Soundness argument

The Lean theorems are about interleavings of whole statements. Threads interleave finer: two
chains' machine instructions run at the same time. The argument that this changes nothing:

1. Chains of one level have disjoint footprints. That is what `check` verifies.
2. Every location a statement touches is in its footprint: its variables, its fields, and, through
   effects, what it reaches through interior mutability and the outside world.
3. So no location is accessed by two chains unless both only read it. The execution is
   *data-race-free*, and safe Rust's memory model guarantees that a data-race-free execution
   behaves as some interleaving of its steps.
4. Any interleaving of steps with disjoint footprints can be reordered, one commuting step at a
   time, into whole statements in a level's order. `level_correct` covers the rest.

Step 2 is the footprint-soundness obligation of Phase I, plus one new effect, *thread*. Step 4 is
a small extension of `Schedule.lean`: operations as sequences of micro-steps. That proof is part
of this work.

## Experiments for the prototype and the audit

1. **Microbenchmarks.** The cost classes' thresholds: pool handoff, adaptive loops on a cached
   and a cold file system, and a slow network through a delay proxy.
2. **The ten P6 findings**, reconstructed as `graph_suite` shapes with blocking timers, and then
   the real code where it builds: rustic against a local and a remote (rclone) repository, and
   tauri's bundler with a cold cache.
3. **The audit (#166).** Run the analysis over the 35 repositories' synchronous functions, and
   report for each candidate: the class, whether facts were needed, the measured gain, and why
   there was none. Thread overhead, contention for one disk, and internal parallelism (pixi
   already hashes with rayon, and each rustic backup already parallelizes internally) are the
   expected reasons. Report the misses as prominently as the
   wins.
