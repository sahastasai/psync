# Environment variables and tools

## `PSYNC_GRAPH`

| Value | Effect |
|---|---|
| `off` | every `#[psync::graph]` function in the build compiles as written |
| unset or anything else | normal rewriting |

The variable is read when the macro expands. Cargo does not track it, so touch the annotated
files, or use a separate `CARGO_TARGET_DIR`, whenever you change it. See
[Comparing builds](../guide/comparing.md).

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
