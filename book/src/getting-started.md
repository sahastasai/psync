# Getting started

This chapter takes you from an empty project to a function that psync runs concurrently. Then it
shows how to see what psync did, and how to turn it off.

## Requirements

- Rust 1.88 or newer. The analysis crate is written in edition 2024 with `let` chains, and the
  generated loop code uses async closures, which became stable in 1.85.
- An async runtime of your choice. The generated code needs only `core` and `alloc`, so Tokio,
  smol, embassy or psync's own executor all work.

## Add the dependency

psync is not on crates.io yet. Depend on a checkout of the repository:

```toml
[dependencies]
psync = { path = "../psync", default-features = false }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
```

> **Which version.** This book describes the development branch, which is not published yet.
> The branch `claude/initial_psync_graph` on GitHub
> (`psync = { git = "https://github.com/sahastasai/psync", branch = "claude/initial_psync_graph" }`)
> has an earlier `#[psync::graph]`, with the scheduling of the Guide but without side effects,
> `#[psync::speculate]`, `strict`, `effects(..)`, the `MultiExecutor` or `cargo psync`. Everything
> in [Safety](./safety/effects.md) needs the development branch.

On a desktop or server, use `default-features = false`. The default feature, `rp235x`, adds the
RP2350 heap and hardware support, and you only want that for firmware.

## Your first concurrent function

Here are three requests that do not depend on each other:

```rust,ignore
use std::time::{Duration, Instant};
use tokio::time::sleep;

async fn fetch(name: &str, ms: u64) -> Result<String, String> {
    sleep(Duration::from_millis(ms)).await;
    Ok(format!("{name} ({ms} ms)"))
}

#[psync::graph(explain)]
async fn dashboard() -> Result<Vec<String>, String> {
    let user = fetch("user", 100).await?;
    let orders = fetch("orders", 100).await?;
    let weather = fetch("weather", 100).await?;
    Ok(vec![user, orders, weather])
}

#[tokio::main]
async fn main() {
    let started = Instant::now();
    let parts = dashboard().await.unwrap();
    println!("{parts:?} in {:?}", started.elapsed());
}
```

`explain` makes psync print the dependency graph while it compiles the function:

```text
psync::graph `dashboard`:
  region 0:
    n0: user = fetch()? [await] {read service "user" (fetch)}
    n1: orders = fetch()? [await] {read service "orders" (fetch)}
    n2: weather = fetch()? [await] {read service "weather" (fetch)}
    level 0: n0 || n1 || n2
```

Each statement is a node (`n0`, `n1`, `n2`). No edges are listed, so nothing waits for anything
else, and all three nodes run in one level (`||` means "at the same time"). The program prints:

```text
["user (100 ms)", "orders (100 ms)", "weather (100 ms)"] in 101.297078ms
```

Now build the same program with psync turned off:

```sh
PSYNC_GRAPH=off cargo run
```

```text
["user (100 ms)", "orders (100 ms)", "weather (100 ms)"] in 303.846918ms
```

You get the same result in about a third of the time. Both outputs above are from real runs.

> **Cargo does not track `PSYNC_GRAPH`.** If you switch the variable between builds, touch the
> annotated files (or use separate `CARGO_TARGET_DIR`s). Otherwise Cargo reuses the previous
> expansion. [Comparing builds](./guide/comparing.md) has a recipe.

## Running on psync's executor

The same functions run on psync's own executor. Here a hand-made future stands in for I/O. It
stays pending for a number of polls:

```rust,ignore
use std::{cell::Cell, future::poll_fn, rc::Rc, task::Poll};

async fn work(polls: u32, value: u32) -> u32 {
    let mut left = polls;
    poll_fn(move |cx| {
        if left == 0 {
            return Poll::Ready(value);
        }
        left -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}

#[psync::graph]
async fn sum() -> u32 {
    let a = work(10, 1).await;
    let b = work(10, 2).await;
    a + b
}

fn main() {
    let executor = psync::Executor::default();
    let out = Rc::new(Cell::new(0));
    let o = out.clone();
    executor.spawn(1, async move { o.set(sum().await) });
    executor.run_until_stalled();
    println!("sum = {}", out.get()); // sum = 3
}
```

[The executor](./guide/executor.md) explains priorities, waking, and running on the RP2350.

## Seeing the code psync generates

There are two ways to look at what psync did:

- `#[psync::graph(explain)]` prints the graph at compile time, as shown above.
- `psync-expand` applies the attribute to any function in any Rust file. It prints the generated
  code and the graph as JSON. You do not have to annotate the function first:

```sh
cargo run -p psync-graph --features cli --bin psync-expand -- path/to/file.rs LINE [BOUND]
```

`LINE` is any line inside the function. [Reading the report](./guide/explain.md) explains the
output.

## Turning it off

- `#[psync::graph(off)]` compiles one function as written. Hint attributes are still removed.
- Building with the environment variable `PSYNC_GRAPH=off` does the same for every annotated
  function.

## Running the project's own checks

From a clone of the repository:

```sh
cargo test --no-default-features --test graph            # behaviour tests for the macro
cargo test --no-default-features --test scheduling       # executor tests
cargo run --example graph_suite --no-default-features    # 16 real-world shapes, timed
```
