Sahanav *Sai Ram*esh

*Dedicated to Baba.*

# psync

A `no_std`, heap-backed, cooperative async executor for RP2350 ARM cores.
Tasks execute by priority only (larger `u16` values first), with FIFO rotation
among ready tasks of equal priority. Sleeping tasks are only polled after a wake.
There is no dependency enumeration or preemption; a continuously ready higher
priority task can starve lower priorities. Keep each executor on one core;
wakers and task handles can be sent across threads/interrupts.

```rust
let executor = psync::Executor::default();
let handle = executor.spawn(10, async { /* work */ });
handle.set_priority(20);
executor.run_until_stalled(); // or executor.run() to park and wait forever
```

For RP2350, call `unsafe { psync::init_heap() }` exactly once before allocating.
The supplied memory map assumes 4 MiB flash (Pico 2) and reserves 32 KiB for the
stack. Firmware must include the RP2350 image definition and cortex-m-rt linker
script; `examples/rp2350.rs` demonstrates both through the example build setup.
The old peripheral/core/context-switch sketches are not part of this runtime.

Run host checks and build a linked RP2350 smoke test:

```sh
rustup target add thumbv8m.main-none-eabihf
./scripts/test-rp2350.sh
```

The script requires Cargo, the ARM target, and a Rust toolchain including
`rust-lld`. Add `--flash` to download with `probe-rs` and an attached debug probe.
After reset, the smoke test's `RESULT` symbol is 1 on success or 2 on panic;
flashing alone does not assert a hardware pass. The firmware checks descending
priority execution. Host tests additionally check fair rotation, wake retention,
priority changes, and spawning from a running task. `cargo test
--no-default-features` checks the portable executor without RP2350 support.

## Automatic concurrency: `#[psync::graph]`

`#[psync::graph]` rewrites an async function so that awaits which do not depend on each other run
concurrently, as the psync proposal's Phase I describes. It builds the function's dependency
graph from its tokens (bindings, field-level reads and writes, ordering of ordinary statements),
runs independent chains together, and prefetches loop iterations whose awaited work does not
depend on earlier iterations. Results, errors and early exits stay as in the sequential code;
`?`, `return`, `break` and `continue` are performed in program order after the overlapped work.

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

Hints cover what tokens cannot show: `#[psync::seq]` keeps a statement in place,
`#[psync::fresh]` gives each branch its own value of a binding (such as a pooled connection), and
`#[psync::independent]` asserts that a loop's iterations do not depend on each other.
`#[psync::graph(explain)]` prints the graph at compile time; `#[psync::graph(off)]` compiles the
function as written. The analysis lives in `psync-graph`; the attribute in `psync-macros`.

The generated code uses only `core`/`alloc` and runs on any executor, including psync's.

```sh
cargo test --no-default-features --test graph                          # behaviour tests
cargo run --example graph_suite --no-default-features                   # 16 real-world shapes
cargo run -p psync-graph --features cli --bin psync-expand -- FILE LINE # rewrite any function
```

`graph_suite` reconstructs functions from popular repositories (agave, matrix-rust-sdk, loco,
Lemmy, delta-rs, DataFusion, mise, sqlx, iggy, kube, neon, quickwit, GreptimeDB, Lighthouse, Zed),
with I/O replaced by timers of realistic latency. For each it prints the original code and its
runtime, the code psync generates and its runtime, and the speedup, and checks that both return
the same result. `psync-expand` applies the attribute to the function containing LINE of any Rust
file and prints the generated code and graph as JSON.

## Local demo

```sh
cargo run --example priority_demo --no-default-features
```

This deterministic terminal demo runs the real executor through device startup,
a simulated sensor interrupt, and an urgent buffer flush. It prints every task
poll and asserts the expected ordering, showing priority scheduling, rotation
between equal priorities, sleeping/waking, and live priority changes. The sensor
interrupt is simulated by a host thread; no board or other async runtime is needed.
