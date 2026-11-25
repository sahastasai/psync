# The audit: where Phase II helps

> **Status: done** for the corpus scan and two projects, rustic and tauri-bundler.

The question is not whether threads can make synchronous code faster: they can. It is how often
psync, as built, finds a place where they do, and whether that place is worth it. This chapter
records the answer, including where the answer is "nowhere".

## The corpus scan

`psync-scan` applies `#[psync::graph]` with default options to every function of a set of files,
as if each one were annotated, and reports every region where **two waiting statements share a
level in different chains**: two awaits, or two blocking calls. A region where a wait only runs
beside a `Vec::new()` gains nothing, so it does not count.

```sh
cargo build --release -p psync-graph --features cli --bin psync-scan
find repos -name '*.rs' | xargs target/release/psync-scan --sync  > sync.tsv
find repos -name '*.rs' | xargs target/release/psync-scan --async > async.tsv
```

Over the 43 repositories cloned for the [audit](../introduction.md#where-psync-is-going) (36,526 files):

| | Functions | Functions with two overlapping waits | Outside tests, benches, examples, build scripts |
|---|---|---|---|
| async | 96,360 | 14,641 (15,809 regions) | 7,100 functions in 42 repositories |
| synchronous | 427,313 | 0 | 0 |

Without hints, **Phase II finds nothing**. The 460 regions the scan reports inside synchronous
functions are all async blocks (`Box::pin(async move { .. })`, `cx.spawn(async move { .. })`),
which are Phase I code. There are three reasons, and they are the same three seen by hand in the
audit's ten synchronous findings:

1. **Processes are ordered.** Most recognizable blocking calls start processes (`git`, `xcrun`,
   `codesign`, `vswhere`), and a child process may touch anything, so two of them never overlap.
   tauri's `build_tools_version` runs `vswhere` twice, as two read-only queries, but nothing in
   the code says that the runs do not interfere. `#[psync::independent]` would.
2. **Requests hide in helpers.** The slow calls of the audit's findings are `download(url)`,
   `repo.infos_files()`, `opts.backup_snapshot(..)`. None of them names the network, so without a
   hint psync cannot know that they wait.
3. **A recognizable network call rarely has a sibling.** Code that opens a `TcpStream` usually
   does one thing with it.

So Phase II, as a source transformation, needs to be told where the time goes:
`#[psync::par]` on the statement, or compiler facts that a callee reaches the network (the
design's next step). That is unlike Phase I. There, `.await` marks every wait, and the same scan
finds 7,100 production functions with independent awaits.

## What the scan found in psync itself

Running psync over 36,000 files of other people's code also tested psync, and found three bugs,
all fixed:

- **A crash in the loop rewriter**, in 52 functions of DataFusion, greptimedb and lighthouse. A
  loop body that ends in an await without a semicolon (`.run_test(..).await`, valid Rust because
  its value is `()`) produced invalid code, and `#[psync::graph]` failed to compile. Chains now
  end every member with a `;`. The test is `loop_bodies_may_end_in_an_await` in
  `tests/graph.rs`.
- **Blocking statements recognized too loosely.** The first version treated any statement
  mentioning `Command` as a process, including clap's `Command::parse_from` and
  `Command::Reply`, an enum variant. Only a path *and* a waiting method count now.
- **Blocking calls inside macros.** Two `assert!(Command::new(..).status()..)` statements were
  recognized as blocking, and since psync cannot see effects inside macro arguments, they would
  have run side by side with no effects at all. Macro arguments are no longer scanned for blocking
  calls.

## Measured on real code

### rustic, built with psync

rustic at commit `143d073`, built twice: as released, and with the edits a user would make
(`scripts/audit/rustic/`):

- `repoinfo`: the struct literal `Infos { files: .., index: .. }` becomes two `let`s marked
  `#[psync::par]`, because psync schedules statements, not struct fields. `repo.open(..)` takes
  the repository by value while the other statement borrows it, so it opens a clone:
  `Repo(repo.0.clone()).open(..)`.
- `diff`: the two snapshot lookups are marked `#[psync::par]`.

Both run against a REST repository of 20 snapshots, behind toxiproxy, which adds latency to every
response. Both binaries print the same output at every latency, and the harness checks this before
timing. Medians of 11 runs, on the shared 32-thread machine (load average about 45):

| Added latency | `repoinfo` sequential | psync | Speedup | `diff` sequential | psync | Speedup |
|---|---|---|---|---|---|---|
| 0 ms | 372 ms | 366 ms | 1.02× | 384 ms | 390 ms | 0.98× |
| 5 ms | 408 ms | 440 ms | 0.93× | 594 ms | 551 ms | 1.08× |
| 20 ms | 578 ms | 539 ms | 1.07× | 951 ms | 961 ms | 0.99× |
| 50 ms | 867 ms | 668 ms | **1.30×** | 1725 ms | 1740 ms | 0.99× |

These are smaller than the 2.00× of the `repoinfo` shape in `examples/phase2.rs`, and the reason
is the point of this audit:

- **`repoinfo` gains 1.30× at 50 ms, and nothing on a local disk.** Both statements run in
  parallel, but the command also opens the repository (config, keys, snapshot list) before them,
  and that part stays sequential. The shape in `examples/phase2.rs` had only the two statements.
- **`diff` gains nothing.** The server's log shows 46 requests, the same 46 for both binaries.
  The two lookups psync overlaps are 2 of them. The remaining 18 data reads come from comparing
  the two trees, inside `diff(repo.ls(&node1)?, repo.ls(&node2)?, ..)`, which streams one tree
  against the other, and psync cannot split a stream. At most one round trip is saved, about 3%,
  which is less than the noise.

The `diff` edit also exposed a psync miss, now fixed. `let path2 = path2.unwrap_or(path1);`
sits between the two lookups and feeds the second one. psync did not know `unwrap_or`, so the
statement was its own chain, and it pushed the second lookup into a later level than the first.
`unwrap_or` with pure arguments is now pure (test `cheap_statements_do_not_delay_waits`).

### tauri-bundler, built with psync

`prepare_tools` in tauri-bundler, at commit `548a369`, fetches the tools that build an AppImage:
three downloads, each skipped if the file is already in the cache. It was built three ways
(`scripts/audit/tauri/`):

- **seq**: as released;
- **par**: `#[psync::graph]` on the function and `#[psync::par]` on the three
  `if !path.exists() { download; write }` statements, which is what a user marks first;
- **spec**: `par` and `#[psync::speculate]` on the second and third, built with
  `cargo psync build`, so the macro schedules with the compiler's facts;
- **hinted**: `spec`, and also `#[psync::independent]` on the three, built without facts.

`par` alone overlaps nothing, and psync is right not to. Each statement calls `download(..)` and
`write_and_make_executable(&path, ..)`, two helpers psync cannot see into. As far as it knows, the
first statement's write may change what the second reads. The hints say what the code means:
each statement writes only its own cache file, and filling a cache is harmless even if an earlier
download fails.

The real code needed two more fixes in psync before even `hinted` overlapped. Between the
downloads sit `let linuxdeploy = tools_path.join(..)` and
`let linuxdeploy_arch = if arch == "i686" { "i386" } else { arch };`. psync did not know that
`Path::join` and an `if` that picks between two values are pure, so it kept them in order with the
downloads. Once they were pure, the second download had two edges into it, one from each binding.
A node joined a chain only when it had a single predecessor, so the download was left in a later
level. A node now joins the chain that ends in its last predecessor when all its edges come from
that chain, because members of a chain run in order (test
`path_joins_and_pure_ifs_do_not_delay_downloads`). `join` with one argument is pure, and
`handle.join()`, which waits for a thread, is not.

With the compiler's facts, the `independent` hint is no longer needed. The driver finds that
`write_and_make_executable(path, ..)` writes the file passed as its parameter 0
([per-argument effects](../internals/compiler.md#what-a-summary-is)). The macro takes that file
from the call, `&apprun`, and from `let apprun = tools_path.join(format!("AppRun-{arch}"))` it
knows the target is `{tools_path}/AppRun-{arch}`, which differs from
`{tools_path}/linuxdeploy-…` ([targets](../safety/effects.md#when-two-effects-conflict)). So
the three writes are to three files. `speculate` stays: whether filling a cache before an earlier
download has failed is acceptable is about intent, not about effects.

The compiler pass checks the schedule and reports one warning. In MIR, the two paths are values
computed by `join` and `format!`, so the driver cannot tell them apart. The macro tells them
apart from the tokens, under the assumption above that different names in one directory are
different files. This is the one place in the audit where the macro knows more than the
checker.

The benchmark runs `prepare_tools` on an empty cache, against a local mirror of the GitHub
release files (through tauri's own `TAURI_BUNDLER_TOOLS_GITHUB_MIRROR`), with files of the real
sizes (18 KB, 12 MB, 9 MB). toxiproxy adds latency, and in two rows caps each connection's
bandwidth. All four builds write the same files at every setting. Medians of 21 runs, on the
shared machine (load average about 40):

| Latency | Bandwidth per connection | seq | par | spec | hinted | par | spec | hinted |
|---|---|---|---|---|---|---|---|---|
| 0 ms | unlimited | 80 ms | 60 ms | 43 ms | 43 ms | 1.34× | 1.84× | 1.85× |
| 20 ms | unlimited | 120 ms | 120 ms | 59 ms | 55 ms | 1.00× | **2.05×** | **2.18×** |
| 100 ms | unlimited | 359 ms | 360 ms | 135 ms | 136 ms | 1.00× | **2.66×** | **2.65×** |
| 100 ms | 20 MB/s | 1385 ms | 1385 ms | 720 ms | 720 ms | 1.00× | **1.92×** | **1.92×** |
| 100 ms | 5 MB/s | 4537 ms | 4558 ms | 2540 ms | 2521 ms | 1.00× | **1.79×** | **1.80×** |

`spec` and `hinted` build the same schedule, and their times agree. The 0 ms row is within the
noise of the machine: an earlier run of the same binaries measured 57 ms for `seq`.

This is the best case the audit found: a command that is a few independent requests and little
else. With a bandwidth cap, the parallel time is bounded by the largest file (12 MB at 5 MB/s is
2.4 s), so the gain falls toward the ratio of the total to the largest. The cap here applies to
each connection. On a link that is itself the bottleneck, parallel downloads share it, and the gain
would be smaller.

### The shapes

The audit's ten synchronous findings, rebuilt as shapes over real sockets in
`examples/phase2.rs`, are in [Synchronous code](./using.md#measurements): 2.0× to 3.0× where
the downloads or reads are independent, 1.00× (correctly) for tauri's code as written, and a
slowdown where a statement is cheaper than a thread.

## What this means

Phase II works: the code it runs in parallel is correct, and the overlap is real. But it helps
real synchronous code much less than Phase I helps async code. It needs hints to find anything,
and where it finds something, other sequential work in the same command dilutes the gain. The
largest gains sit where a command is a few independent requests and little else, as in tauri's
`prepare_tools`: 2.0× to 2.7× on the real code once latency matters. With `cargo psync build`, it
takes one hint, `speculate`, which states intent. The effects come from the compiler.
