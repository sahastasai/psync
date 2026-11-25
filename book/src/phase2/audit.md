# The audit: where Phase II helps

> **Status: in progress.** The corpus scan and rustic are done; a second project is next.

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
`prepare_tools` (3 downloads, 2.98× as a shape).
