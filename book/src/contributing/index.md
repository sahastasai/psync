# A tour of the repository

This part is for people who want to change psync. It assumes the
[Foundations](../foundations/index.md) part, or the same background. It shows where everything
lives, how a change travels through the code, how to set up a development environment, and how
to check a change before you send it.

## The crates

| Path | What it is | Built with |
|---|---|---|
| `psync-macros/` | the proc-macro crate: `#[graph]` and the hint attributes, all thin wrappers | stable |
| `psync-graph/` | the analysis and code generation behind `#[graph]`, as an ordinary library; the `psync-expand` tool; the Foundations examples | stable |
| `src/` (crate `psync`) | what users depend on: re-exports the macros, the runtime the generated code calls (`graph_rt.rs`, as `psync::__private`), and the executors | stable, `no_std` |
| `psync-check/` | `cargo psync` and `psync-driver`, the compiler-level checks and facts | the pinned nightly |
| `verify/` | the Lean proofs and the Lean tutorial | Lean 4.22 |
| `tests/` | behaviour tests of the macro and the executors | stable |
| `examples/graph_suite/` | sixteen real-world shapes, each run sequentially and with psync | stable |
| `book/` | this book (mdBook) | `mdbook` |

`psync-check` is outside the Cargo workspace (`exclude` in the root `Cargo.toml`), because it
links rustc's internal crates and needs its own toolchain.

## The life of an annotated function

Follow one `#[psync::graph] async fn` through the code, in the order things happen.

1. **`psync-macros/src/lib.rs`** `graph(attr, item)` converts the token streams and calls
   `psync_graph::expand`.
2. **`psync-graph/src/lib.rs`** `expand` parses the options (`Options::parse`), honours `off` and
   `PSYNC_GRAPH=off`, and calls `transform_fn`. That builds the `Scope` (which bindings are `mut`,
   `&mut` handles or `#[psync::fresh]`), loads the compiler facts if `PSYNC_FACTS` is set
   (`facts.rs`), runs the `Rewriter`, strips the hints, and adds the `__PSYNC_GRAPH` marker.
3. **`rewrite.rs`** `Rewriter` is a `VisitMut`. For each block of async code, innermost first, it
   sends `for` and `while` loops to `loops.rs`, then calls the hoisting and scheduling steps.
4. **`hoist.rs`** splits statements so each hidden await becomes its own binding.
5. **`classify.rs`** decides for each statement: node or barrier, and for a node, what it binds,
   whether it awaits, may exit or may panic, and its effects (from `effects.rs`) and accesses (from
   `access.rs`).
6. **`schedule.rs`** `plan` draws the edges (`depends`), fuses chains and assigns levels. `emit`
   generates each level's join, with `exits.rs` providing the code that records exits and replays
   them.
7. **`src/graph_rt.rs`** is what the generated code calls at run time: `maybe_done_at` (a chain),
   `over_at` and `join_all_bounded` (a prefetched loop), and `Branch` and `Rethrow` (the `?`
   plumbing).

`#[graph(explain)]` prints the report that steps 5 and 6 build (`Report` in `lib.rs`), and
`psync-expand` prints it as JSON with the generated code.

## The life of `cargo psync build`

1. **`psync-check/src/bin/cargo-psync.rs`** runs `cargo check` in `target/psync`, with
   `RUSTC_WRAPPER` set to `psync-driver` and the environment variables in `psync_check::env`.
2. **`src/bin/psync-driver.rs`** is rustc with a callback. After analysis it runs the `Checker`
   and the `Analyzer` from `src/driver/analysis.rs`.
3. **`analysis.rs`**: `Analyzer::summarize` computes summaries from MIR. Each body's own effects
   come from `local` and `call`, and summaries are fixed points over call-graph components
   (`component`). `call_facts` writes a fact for every call site in annotated functions, and
   `write_summaries` writes the crate's summary file. `Checker::run` finds the macro's markers
   (`markers`) and checks each level and loop prefix.
4. **`src/lib.rs`** (`psync_check`, no compiler dependency) holds the shared model: `Effect`,
   `conflict`, the summary and findings file formats, and `link`, which resolves deferred `dyn`
   calls over the whole build. `facts_text` writes `facts.tsv`.
5. Back in **`cargo-psync.rs`**, `check_and_link` re-runs the check when linking resolved new
   calls. `build` then runs one more check pass with `PSYNC_FACTS` pointing at `facts.tsv`. In it,
   the macro (step 2 above) schedules with the facts, and the driver checks the schedule that
   results. Last, it runs a plain `cargo build` (no driver, the normal target directory) with the
   same `PSYNC_FACTS`, so the binary gets the schedule that was just checked.

## Setting up

```sh
git clone <the repository> && cd psync
cargo test --no-default-features        # the macro and the single-core executor
```

The default feature `rp235x` builds the embedded executor for the RP2350. On a host,
`--no-default-features` (and `--features std` for `MultiExecutor`) is what you want.

For `psync-check` you need rustup, which reads `psync-check/rust-toolchain.toml` and installs the
pinned nightly with `rustc-dev` on first use:

```sh
cd psync-check
cargo test                    # builds the driver and runs it on the fixture
cargo install --path . --locked   # puts cargo-psync and psync-driver on your PATH
```

rust-analyzer needs `rustc_private` support to understand the driver. `psync-check/Cargo.toml`
sets `[package.metadata.rust-analyzer] rustc_private = true`. Open `psync-check/` as its own
project so the pinned toolchain is used.

Lean: install `elan`. `lean verify/Schedule.lean` fetches nothing, because the proofs use only
Lean's core library.

The book: `cd book && mdbook build`, then open `book/book/index.html`.

## Before you send a change

Run all of these. Each takes from seconds to a couple of minutes.

```sh
cargo test --no-default-features                                    # graph + scheduling tests
cargo test --no-default-features --features std --test multi        # MultiExecutor
cargo test -p psync-graph                                           # analysis unit tests, Foundations examples
cargo run --example graph_suite --no-default-features -- --brief    # speedups: geometric mean ~3.8x
(cd psync-check && cargo test)                                      # driver: check, --facts, fix
lean verify/Schedule.lean && lean verify/Tutorial.lean              # proofs
(cd book && mdbook build)                                           # the book
```

If your change alters what psync does, it changes the book too. Update the chapter that
describes the behaviour, and the report output it shows. The rest of this part gives the
recipes for the most common changes:

- [Adding an effect rule](./effect-rule.md): teach the macro or the driver what a call does.
- [Adding a check](./check.md): a new warning in `cargo psync check`.
- [Adding a test](./tests.md): which suite, and how.
