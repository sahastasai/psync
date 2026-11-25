# psync-check: `cargo psync check`

`#[psync::graph]` judges calls by their names, because a proc macro sees only tokens.
`cargo psync check` judges them by their bodies. It runs the compiler with a custom driver
(`psync-driver`, built like `clippy-driver`) that:

1. computes, from MIR, what every function of every crate in the build does to files, sockets,
   processes, the environment and other crates' APIs, following calls into function bodies and
   across crates;
2. finds the code `#[psync::graph]` generated (it is tagged with the macro's assumptions) and warns
   where a rewrite
   - runs two conflicting effects at once (`conflict`),
   - performs a write before an earlier statement that may exit (`speculative-write`),
   - prefetches loop items whose prefix writes, in a loop that can end early
     (`loop-speculative-write`),
   - and, with `--notes`, where concurrent chains share a `Cell`/`RefCell`/`Mutex`/atomic or both
     print.

```text
warning: psync runs this concurrently with other work that has a conflicting effect: this does
         `read file "cache.json"` (via load -> std::fs::read_to_string), the other does
         `write file "cache.json"` (via warm -> std::fs::write)
  --> app/src/lib.rs:24:20
   |
24 |     let s = load().await?;
   |                    ^^^^^
note: the conflicting effect
  --> app/src/lib.rs:23:12
   |
23 |     warm().await?;
   |            ^^^^^
```

## Install and run

The driver links rustc's internal crates, so it is built with the nightly pinned in
`rust-toolchain.toml` (rustup installs it, with `rustc-dev`, on first build).

```sh
cd psync-check
cargo install --path . --locked      # cargo-psync and psync-driver
cd /your/project
cargo psync check                    # or: --deny for CI, --notes for notes
cargo psync summaries [CRATE]        # each function's effects
```

`cargo psync check` is `cargo check` in `target/psync` with `psync-driver` as `RUSTC_WRAPPER`. The
normal build is untouched. Unchanged crates are not recompiled (cargo replays their warnings).

## Tests

```sh
cargo test        # runs `cargo psync check` on tests/fixtures/hidden and checks every finding
```

## Layout

| | |
|---|---|
| `src/lib.rs` | the effect model, summary and findings files (no compiler dependency) |
| `src/driver/analysis.rs` | MIR summaries and the checks |
| `src/bin/psync-driver.rs` | the rustc driver |
| `src/bin/cargo-psync.rs` | the cargo subcommand |
| `tests/fixtures/hidden` | a workspace whose rewrites hide effects in callees |

The book's chapter [Checking rewrites](../book/src/safety/check.md) explains the analysis and its
limits.
