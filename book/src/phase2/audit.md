# The audit: where Phase II helps

> **Status: in progress.** The corpus scan below is complete. Measurements on real projects
> follow as they are built.

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

The audit's ten synchronous findings, rebuilt as shapes over real sockets in
`examples/phase2.rs`, are in [Synchronous code](./using.md#measurements): 2.0× to 3.0× where
the downloads or reads are independent, 1.00× (correctly) for tauri's code as written, and a
slowdown where a statement is cheaper than a thread. Building the projects themselves with psync
is the next step of this audit.
