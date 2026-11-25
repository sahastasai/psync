# Environment variables and tools

## `PSYNC_GRAPH`

| Value | Effect |
|---|---|
| `off` | every `#[psync::graph]` function in the build compiles as written |
| unset or anything else | normal rewriting |

The variable is read when the macro expands. Cargo does not track it, so touch the annotated
files, or use a separate `CARGO_TARGET_DIR`, whenever you change it. See
[Comparing builds](../guide/comparing.md).

## `cargo psync`

```sh
cargo psync check [--deny] [--notes] [--facts] [CARGO CHECK ARGS...]
cargo psync build [--deny] [--notes] [CARGO BUILD ARGS...]
cargo psync fix [CARGO CHECK ARGS...]
cargo psync summaries [CRATE] [--all]
```

Checks `#[psync::graph]` rewrites against the compiled bodies of callees. Built from `psync-check/`
with a pinned nightly (`cd psync-check && cargo install --path .`). See
[Checking rewrites](../safety/check.md).

| Variable | Set by `cargo psync` | Meaning |
|---|---|---|
| `PSYNC_SUMMARY_DIR` | `target/psync/summaries` | where the driver writes and reads per-crate effect summaries |
| `PSYNC_FINDINGS_DIR` | `target/psync/findings` | where the driver writes per-crate findings (JSON) |
| `PSYNC_CHECK_DENY` | `--deny` | `1`: findings are errors |
| `PSYNC_CHECK_NOTES` | `--notes` | `1`: also report output order and interior mutability |
| `PSYNC_LINKED` | `target/psync/linked.json` | the driver reads the link step's resolution of `dyn` calls |
| `PSYNC_LINKED_STAMP` | hash of `linked.json` | tracked by the driver, so workspace crates are re-checked when it changes |
| `PSYNC_FACTS` | `target/psync/facts.tsv` (`build`, `check --facts`) | the macro schedules with these facts ([Scheduling with compiler facts](../safety/facts.md)) |
| `PSYNC_FACTS_STAMP` | hash of `facts.tsv` | read through `option_env!` in generated code, so cargo rebuilds when facts change |

## `psync-expand`

```sh
cargo run -p psync-graph --features cli --bin psync-expand -- FILE LINE [BOUND]
```

Applies `#[psync::graph]` to the innermost function containing `LINE` of `FILE` and prints JSON:
`function`, `start_line`, `end_line`, `bound`, `changed`, `explain`, `report` and `expanded`. The
`cli` feature enables span locations, so line numbers appear in reports. See
[Reading the report](../guide/explain.md).

## `graph_suite`

```sh
cargo run --example graph_suite --no-default-features [-- --brief] [-- FILTER]
```

Times 16 real-world functions before and after the rewrite, and checks that the results match. See
[Sixteen real-world shapes](../case-studies/graph-suite.md).

## Tests

```sh
cargo test --no-default-features --test graph        # macro behaviour
cargo test --no-default-features --test scheduling   # executor
cargo test -p psync-graph                            # analysis unit tests
./scripts/test-rp2350.sh [--flash]                   # host tests and RP2350 firmware build
```

## Cargo features

| Crate | Feature | Effect |
|---|---|---|
| `psync` | `rp235x` (default) | RP2350 heap (`init_heap`), `WFE`/`SEV` parking and HAL dependencies, on `thumbv8m.main-none-eabihf` only |
| `psync-graph` | `cli` | the `psync-expand` binary, line numbers, JSON reports |
