Sahanav *Sai Ram*esh

*Dedicated to Baba.*

# psync

psync makes sequential `async` Rust concurrent. Put `#[psync::graph]` on an async function, and
the awaits that do not depend on each other run at the same time. Results, errors, early exits and
writes to the outside world stay as they were in the sequential code.

```rust
#[psync::graph(bound = 8)]
async fn check_files(store: &Store, files: &[String]) -> Result<(), Error> {
    for file in files {
        match store.head(file).await {          // up to 8 HEADs in flight
            Ok(_) => {}
            Err(e) => return Err(e.into()),     // still the first failing file, in order
        }
    }
    Ok(())
}
```

The crate also contains a small `no_std`, priority-based cooperative executor for RP2350 cores.
The generated code runs on any executor, including tokio and psync's own.

**Read the book:** `book/` ([build it](#the-book)) teaches psync from a first function to the
internals: how the graph is built, the generated code, what is checked and what is assumed.

## In numbers

| | |
|---|---|
| **9.2×** | best kube-rs application, end to end (`dynamic_api`, remote API server) |
| **4.1×** | best DataFusion CLI session, end to end (S3 Standard latency) |
| **89 of 90** | real-world findings rewritten (79 automatically, 10 with one-line hints) |
| **3.79×** | geometric mean over 16 functions reconstructed from popular repositories |

## How it works

`#[psync::graph]` reads the function's tokens. It does not call the compiler.

1. **Classify** each statement as a movable node, or a barrier that stays in place: locks,
   permits, sleeps, clock reads, `#[psync::seq]`.
2. **Find dependencies.** Edges come from bindings, from field-level reads and writes, from the
   order of ordinary statements, and from **effects** on files, services, the network, processes
   and the environment.
3. **Schedule.** Independent chains of awaits run together as a join. Bindings come back in
   program order.
4. **Loops.** The awaited prefix of each iteration runs up to `bound` items ahead. Loop-carried
   state is split into a first pass (fission).
5. **Exits.** `?`, `return`, `break` and `continue` inside concurrent work are recorded and
   replayed in program order. The function leaves from the same place with the same value.

The Rust compiler still type- and borrow-checks everything psync generates. Two branches that both
need `&mut x` fail to compile; they never race.

## Effects and safety

psync recognizes operations on the outside world from the call: `fs::write(path)`,
`api.delete(name)`, `s3.put_object()`, `client.get(url)`, `Command::new(..)`, `env::set_var`.

- **Operations that conflict keep their order:** a read and a write of the same file, two writes
  to one service, anything next to a process.
- **Writes are not speculated.** A recognized write never starts before an earlier statement that
  may exit. A loop whose items write is not prefetched when the loop can end early, because
  sequential code would not have performed those writes.
- **Unclassified calls are reported.** `explain` lists awaited calls psync could not classify.
  `#[psync::graph(strict)]` treats them as writes.

See the book's [Side effects](book/src/safety/effects.md) chapter, and its
[roadmap](book/src/safety/roadmap.md) for stronger guarantees: runtime target checks,
type-checked effect contracts, and an effect sanitizer for tests.

## Options and hints

| | |
|---|---|
| `#[psync::graph(bound = N)]` | at most N loop items in flight (default 16) |
| `#[psync::graph(explain)]` | print the dependency graph at compile time |
| `#[psync::graph(off)]`, `PSYNC_GRAPH=off` | compile as written, for comparisons |
| `#[psync::graph(strict)]` | unclassified awaited calls are writes |
| `#[psync::graph(effects(read(..), write(..), ordered(..)))]` | name your API's verbs |
| `#[psync::seq]` | keep a statement in place |
| `#[psync::fresh]` | each branch evaluates this `let` again (a pooled connection) |
| `#[psync::independent]` | a loop's iterations, or a statement's effects, do not interfere |
| `#[psync::speculate]` | these writes may run before an earlier exit (idempotent operations) |

## Try it

```sh
cargo test --no-default-features --test graph                          # behaviour tests
cargo test -p psync-graph                                               # analysis unit tests
cargo run --example graph_suite --no-default-features                   # 16 real-world shapes
cargo run -p psync-graph --features cli --bin psync-expand -- FILE LINE # rewrite any function
```

`graph_suite` reconstructs functions from agave, matrix-rust-sdk, loco, Lemmy, delta-rs,
DataFusion, mise, sqlx, iggy, kube, neon, Quickwit, GreptimeDB, Lighthouse and Zed, with I/O
replaced by timers of realistic latency. It prints each original and generated function, times
both, and checks that they return the same result. `psync-expand` applies the attribute to the
function containing LINE in any Rust file and prints the generated code and the graph as JSON.

## The book

```sh
nix-shell -p mdbook --run 'mdbook build book'   # or: cargo install mdbook; mdbook build book
# open book/book/index.html
```

## The executor

```rust
let executor = psync::Executor::default();
let handle = executor.spawn(10, async { /* work */ });
handle.set_priority(20);
executor.run_until_stalled(); // or executor.run() to park and wait forever
```

Larger `u16` priorities run first, with FIFO rotation among ready tasks of equal priority. Sleeping
tasks are polled only after a wake. There is no preemption, so a continuously ready high-priority
task starves lower ones. Keep each executor on one core; wakers and task handles may cross threads
and interrupts.

For RP2350, call `unsafe { psync::init_heap() }` exactly once before allocating. The memory map
assumes 4 MiB flash (Pico 2) and reserves 32 KiB for the stack. Firmware must include the RP2350
image definition and the cortex-m-rt linker script. `scripts/test-rp2350.sh` runs the host tests
and builds the `rp2350` smoke-test example (`--flash` downloads it with `probe-rs`). That example
and the `priority_demo` terminal demo are not checked into this branch yet. `cargo test
--no-default-features` checks the portable executor without RP2350 support.

## Layout

| | |
|---|---|
| `src/` | the executor, and `graph_rt.rs`: runtime helpers for generated code |
| `psync-macros/` | the attributes |
| `psync-graph/` | the analysis and code generation (`classify`, `access`, `effects`, `hoist`, `schedule`, `loops`, `exits`, `rewrite`) |
| `tests/graph.rs` | behaviour tests on simulated I/O |
| `examples/graph_suite/` | the 16 real-world shapes |
| `book/` | the psync book |
