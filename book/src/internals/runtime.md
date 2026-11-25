# Runtime support

The generated code calls a few items in `psync::__private` (`src/graph_rt.rs`). They are not a
public API. The macro's output is the only intended caller, and they may change freely. Everything
here uses only `core` and `alloc`.

| Item | Used for |
|---|---|
| `maybe_done(future) -> MaybeDone<Pin<Box<F>>>` | one branch of a level's join |
| `MaybeDone::poll_step(cx) -> bool` | poll if still pending; `true` once the output is available |
| `MaybeDone::take()` | take the output (panics if called before completion, which generated code never does) |
| `join_all_bounded(iter, bound)` | a prefetched loop: run at most `bound` futures, outputs in input order |
| `over(&iter, f)` | type the async closure's parameter from the iterator |
| `maybe_done_at::<LEVEL, ORDER, EXITS, SPEC, INDEP, _>`, `job_at::<..>` (synchronous jobs), `over_at::<LOOP, TERMINATES, SPEC, INDEP, ..>` | what generated code calls: the same helpers, tagged with the macro's assumptions for [`cargo psync check`](../safety/check.md); free at run time |
| `Branch`, `Rethrow`, `ResultResidual`, `OptionResidual` | record `?` and replay it through the caller's `?` |
| `Vec` | re-exported so generated code can name `alloc::vec::Vec` |
| `tick(&counter, every) -> bool`, `yield_now()` | `yield_every = N`: count a loop's iterations in a counter static to that loop, and yield to the executor on every Nth |

## `MaybeDone`

Each branch is boxed with `Box::pin`. That lets the join poll it through `Pin::new` without unsafe
pin projection. It costs one allocation per branch per level, which is small next to an I/O
round trip. A version without allocation would need pin projection in generated code.

## `join_all_bounded`

```rust,ignore
pub struct JoinAllBounded<I: Iterator> where I::Item: Future {
    iter: Option<Box<I>>,
    active: Vec<(usize, Pin<Box<I::Item>>)>,
    results: Vec<Option<<I::Item as Future>::Output>>,
    bound: usize,
}
```

On each poll it:

1. starts new items while fewer than `bound` are active, so a freed slot is refilled at once;
2. polls every active future, and stores finished outputs at their input index;
3. returns all outputs, in input order, when the iterator is exhausted and nothing is active;
4. returns `Pending` if nothing finished during this pass. Nothing new can start until a wake.

It does not stop early on errors. Errors are values recorded in exit slots, and the loop body
replays them in order. Every item's future is therefore run to completion. This is the speculation
described in [Loops](../guide/loops.md).

All generated code names `core` or `#krate::__private`, never `std`, so it compiles in `no_std`
crates that have `alloc` (the RP2350 build is one). Fission's first loop collects into the
re-exported `Vec`.
