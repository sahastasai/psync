# Adding a test

psync has a test suite for each kind of claim it makes. Pick the suite by the question your
test answers:

| Question | Suite | Run |
|---|---|---|
| Does the analysis draw this edge, place this barrier, refuse this loop? | unit tests in `psync-graph/src/*.rs` | `cargo test -p psync-graph` |
| Does the rewritten function return the same thing, and overlap what it should? | `tests/graph.rs` | `cargo test --no-default-features --test graph` |
| Do the executors schedule as documented? | `tests/scheduling.rs`, `tests/multi.rs` | `cargo test --no-default-features`; `--features std --test multi` |
| Does the driver find this effect, report this finding, compute these facts? | the fixture, `psync-check/tests/fixture.rs` | `cd psync-check && cargo test` |
| Does a synchronous function keep its results, errors and panics on threads? | `tests/sync.rs` | `cargo test --no-default-features --features std --test sync` |
| Is this real-world shape faster, and still correct? | `examples/graph_suite/` | `cargo run --example graph_suite --no-default-features -- --brief` |
| How much do synchronous shapes gain, over real sockets? | `examples/phase2.rs` | `cargo run --release --example phase2 --no-default-features --features std` |
| Is the scheduling argument right? | `verify/Schedule.lean` | `lean verify/Schedule.lean` |
| Is it right against a server, relative to a spec? | `verify/World.lean` | `lean verify/World.lean` |
| Is *this* schedule safe? | certificates, `verify/Cert.lean` | `scripts/certify.sh` (regenerate `verify/Certs.lean`, then `lake build`) |
| Do the book's examples still run? | `psync-graph/examples/`, `verify/Tutorial.lean` | `cargo test -p psync-graph`; `lean verify/Tutorial.lean` |

## Analysis decisions: `psync-graph` unit tests

These tests run the analysis on a function built with `syn::parse_quote!` and assert on the
report. They do not compile or run the generated code, so they are fast and precise about *why*:

```rust,ignore
#[test]
fn files_and_services_order_conflicting_awaits() {
    let r = report(
        Options::default(),
        syn::parse_quote! {
            async fn f(api: &Api) -> Result<(), E> {
                tokio::fs::write("cfg.toml", b"x").await?;
                let s = tokio::fs::read_to_string("cfg.toml").await?;
                let o = tokio::fs::read_to_string("other.toml").await?;
                api.delete(name).await?;
                Ok(())
            }
        },
    );
    // ... assert on edges(&r): the write and the read of cfg.toml are ordered,
    //     other.toml is free
}
```

`report` and `edges` are helpers in the `tests` module of `lib.rs`. The report has everything
`explain` prints: regions with nodes, edges and their reasons, levels, barriers with reasons, and
loops with refusals.

## Behaviour: `tests/graph.rs`

These tests compile real annotated functions and run them on psync's `Executor`. Time is counted
in executor polls, so the tests are deterministic. The helper `io(ticks, value)` is a future that
stays pending for `ticks` polls and records how many are in flight. `run(future)` returns the
output, the polls taken and the peak in flight. A typical test writes the function twice, plainly
and with `#[graph]`, and asserts three things:

```rust,ignore
#[test]
fn tree_overlaps_independent_waits() {
    let (seq, seq_polls, seq_peak) = run(sequential_tree());
    let (par, par_polls, par_peak) = run(graph_tree());
    assert_eq!(seq, par);                                  // 1. same result
    assert_eq!((seq_peak, par_peak), (1, 4));              // 2. the concurrency it should have
    assert_eq!(seq_polls, 7 * T as usize + 1);             // 3. the time it should take
    assert_eq!(par_polls, 3 * T as usize + 1);
}
```

For exits, test that the *same error* comes back when several statements fail. For effects and
barriers, test the *order* of side effects, for example by pushing to a shared log and comparing
logs. A test that only checks the result would pass for a rewrite that does nothing.

## The driver: the fixture

`psync-check/tests/fixtures/hidden` is a small workspace (`app` and `helpers`) whose functions
hide effects where the macro cannot see them. `tests/fixture.rs` runs `cargo psync check`,
`check --facts` and `fix` on it, and asserts on the findings files:

- **each finding**, by kind, function and message;
- **the total**, so a new false positive fails the test;
- **with facts**, which functions become sequential and which gain concurrency;
- **fix**, how many hints it inserts, and that the result checks clean.

When you add a case, every one of those numbers may change, and each change should be one you
can explain. [Adding an effect rule](./effect-rule.md) and [Adding a check](./check.md) show
complete examples. The test compiles the fixture from scratch, so expect about a minute.

## Real-world shapes: `graph_suite`

Each shape is a function reconstructed from an open-source project, with its I/O replaced by
timers of realistic latency. The `example!` macro in `show.rs` compiles the function twice, as
`#[psync::graph(off)]` and `#[psync::graph(options)]`, and the suite checks that both give the
same result before it reports the speedup:

```rust,ignore
example! {
    origin: "kube-rs/kube  kube-client/src/discovery/mod.rs  Discovery::run  (infra-08)",
    pattern: "P3 per-item chains",
    summary: "list the API groups, then query each queryable group one after another",
    options: (bound = 16),
    call: |f| f(Discovery { /* .. */ }),
    async fn run(mut discovery: Discovery) -> Result<usize, String> { /* the original code */ }
}
```

Add the module's `example()` to its file's `examples()` list. Keep the code as close to the
original as the stubs allow, and cite the file and function in `origin`. The suite's geometric
mean (about 3.8×) is quoted in the book, so update [Sixteen real-world shapes](../case-studies/graph-suite.md)
when it changes.

## Proofs

A new theorem goes in `verify/Schedule.lean`, with a docstring that says in words what it means
for psync. It must contain no `sorry`. Lean prints a warning for each one, and CI runs
`lean verify/Schedule.lean`. The proofs use only Lean's core library, so the file checks in
about a second with no downloads. Update the theorem table in `verify/README.md` and in
[Proofs](../foundations/proofs.md).
