# Adding a check

A *check* is a warning (or a note) that `cargo psync check` prints when the code
`#[psync::graph]` generated does something the sequential code would not. All checks live in
`Checker`, in `psync-check/src/driver/analysis.rs`. This chapter shows how a check gets its
information, and walks through a real change: treating a callee that may panic as an exit.

## How a check sees the macro's work

The driver does not re-run psync's analysis. It reads what the macro *did*, from the generated
code's MIR, and compares that with what the compiled callees *do*.

1. **Markers.** The macro wraps each concurrent chain in
   `psync::__private::maybe_done_at::<JOIN, ORDER, LAST, EXITS, EXIT_AT, SPEC, INDEP, _>(async { .. })` (in
   synchronous code, each job in `job_at::<..>(|| ..)`), and each
   prefetched loop prefix in `over_at::<LOOP, TERMINATES, SPEC, INDEP, ..>`. `Checker::markers`
   finds these calls in a function's MIR, reads the const generic arguments, and finds the body of
   the `async` block from the last type argument (`coroutine_of`). The result is a list of joins,
   each a list of `Chain { body, order, exits, spec, indep, span }` sorted by program order, and a
   list of `Prefix { body, terminates, spec }`.
2. **Summaries.** `self.analyzer.summarize(chain.body)` gives everything a chain may do:
   effects with their spans and call chains, `panics`, `opaque` callees.
3. **Comparison.** `check_level` looks at every pair of chains in a join, earlier and later in
   program order. `check_prefix` looks at a loop's prefix, and `check_interior` at shared
   `Cell`/`RefCell`/`Mutex`/atomic values.
4. **Reporting.** `self.report(severity, kind, function, span, message, note)` emits a rustc
   diagnostic at `span` (so it appears in the editor like any warning) and records a `Finding`.
   `write_findings` saves those findings to `target/psync/findings/`, where `cargo-psync.rs` prints
   the summary line, and `fix` reads each warning's suggested `#[psync::seq]`.

The macro's tags are the contract between the two halves. If you add a new hint or a new kind of
rewrite, give its marker the facts a check will need, as const generics, which cost nothing at
run time and survive into MIR.

## Worked example: panics are exits

**The gap.** The `speculative-write` check fired when an earlier chain was tagged `EXITS` (it
contains a `?`, `return`, `break` or `continue`) and a later chain writes. But a chain can also
stop the function by *panicking* inside a callee:

```rust,ignore
async fn parse_config() -> usize {
    std::fs::read_to_string("app.toml").unwrap().len()      // panics if the file is missing
}
async fn stamp_run() -> std::io::Result<()> {
    std::fs::write("run.stamp", b"1")
}

#[graph]
pub async fn boot() -> Result<usize, Error> {
    let n = parse_config().await;          // no `?`: the macro sees no exit
    stamp_run().await?;                    // runs alongside it
    Ok(n)
}
```

Sequentially, a missing `app.toml` panics before the stamp is written. With psync, the stamp may
already be on disk when the panic happens. The macro cannot know, because `unwrap` is in another
function. The driver does know: `parse_config`'s summary has `panics: true`. With
`cargo psync build`, the macro gets that fact and orders the two. But plain `cargo psync check`
said nothing.

**The test.** Add `boot` and its helpers to the fixture, and assert the finding in
`tests/fixture.rs`:

```rust,ignore
// A callee that may panic is an exit: the write after it is speculative.
assert!(
    app.findings.iter().any(|f| f.kind == "speculative-write"
        && f.function == "boot"
        && f.message.contains("may panic")),
    "{:#?}", app.findings
);
```

Also raise the total number of findings, add `boot` to the functions that facts make sequential,
and raise the number of hints `cargo psync fix` inserts. The fixture test pins all of these, so a
change in behaviour you did not intend shows up as a failure.

**The change.** In `check_level`, the condition becomes "the earlier chain exits *or panics*",
with a message that says which:

```rust,ignore
let panics = !earlier.exits && summaries[i].panics;
if (earlier.exits || panics)
    && !later.spec
    && let Some(w) = summaries[j].effects.iter()
        .find(|l| l.effect.writes() && l.effect.domain != Domain::Output)
{
    let how = if panics { "may panic" } else { "exits early" };
    self.report(Severity::Warning, "speculative-write", function, w.span,
        format!("psync starts this before knowing whether an earlier statement {how}, ..."),
        Some((user_span(..), "the earlier work that may exit".to_string())));
}
```

Reusing the kind `speculative-write` means `cargo psync fix`, `--deny` and the report need no
change. A new *kind* of problem would get a new kind string, and `cargo-psync.rs` would print it
like the others.

**On real code.** Run `cargo psync check` on a large project before and after. A check that
fires everywhere is worse than none. This one counts only explicit panics in your own code (the
driver does not count panics inside libraries, nor bounds or overflow checks), so it fires
rarely, and only where the fix is warranted.

## Checklist for a new check

- Can the problem be seen from the markers and the summaries? If the check needs a fact the
  macro did not record, add it to the marker first, in `schedule.rs` or `loops.rs` and in
  `graph_rt.rs`.
- Is it a warning (behaviour differs from sequential code) or a note (worth knowing, often fine,
  shown with `--notes`)?
- Does the message say what psync did, what the callee does (`describe()` and `path()` of the
  effect), and which hint fixes it?
- Does `fix` know where to put the hint? `seq_fix` places it before the statement, or before the
  loop for a loop finding.
- Fixture test, real-code run, and the book: [Checking rewrites](../safety/check.md) lists every
  kind of finding.
