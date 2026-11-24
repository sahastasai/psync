# Early exits and errors

A `?` is a branch. You might expect it to stop psync from moving anything past it. It doesn't. psync
lets awaits run concurrently even when they contain `?`, `return`, `break` or `continue`, and still
exits exactly where the sequential code would.

## The rule

Every exit inside a concurrent node is **recorded, not taken**. When the node's chain hits an exit,
the chain stops. After the join, psync goes through the recorded exits **in program order** and
performs the first one. Here is the `first_error_wins` test:

```rust,ignore
#[graph]
async fn first_error_wins(fail: [bool; 3]) -> Result<u32, Error> {
    let x = io(T, if fail[0] { Err(Error("x")) } else { Ok(1) }).await?;
    let y = io(T, if fail[1] { Err(Error("y")) } else { Ok(2) }).await?;
    io(T, if fail[2] { Err(Error("z")) } else { Ok(()) }).await?;
    Ok(x + y)
}
```

All three requests run at once (the test checks a peak of 3 in flight). With `fail = [false, true,
true]` the function returns `Err("y")`. With `[true, false, true]` it returns `Err("x")`. Those are
the errors the sequential function returns.

An exit is replayed only once every statement *before* it has run, because sequential code runs
those first, and one of them might exit instead. That matters when an earlier statement lands in a
later level:

```rust,ignore
let a = io(T, 1).await;
let b = io(T, 2).await;
logged(log, a + b, fail[0]).await?;          // waits for a and b: level 1
let c = io(T, if fail[1] { Err(Error("four")) } else { Ok(4) }).await?;   // level 0
```

`c`'s request overlaps `a`'s and `b`'s. If it fails, psync still runs `logged`, and returns
`logged`'s error if both fail, exactly as the sequential function does. Anything that waits for `c`
starts only after that replay. (The test `exits_wait_for_earlier_statements_in_later_levels`
checks every combination. An earlier version replayed `c`'s exit right after level 0, skipping
`logged`; the Lean theorem `replay_correct` states the rule that this broke.)

## What the generated code looks like

Each `?` becomes a match that stores the residual in a slot and breaks out of the chain:

```rust,ignore
let user = match ::psync::__private::Branch::branch(api.user(user_id).await) {
    Ok(v) => v,
    Err(r) => {
        __psync_e0_0_0 = Some(r);
        break '__psync_chain0 (None);
    }
};
```

After the join, the slots are re-raised in source order through your own `?`:

```rust,ignore
if let Some(r) = __psync_e0_0_0 { ::psync::__private::Rethrow::rethrow(r)?; }   // user
if let Some(r) = __psync_e1_0_0 { ::psync::__private::Rethrow::rethrow(r)?; }   // posts
if let Some(r) = __psync_e0_1_0 { ::psync::__private::Rethrow::rethrow(r)?; }   // avatar
if let Some(r) = __psync_e2_0_0 { ::psync::__private::Rethrow::rethrow(r)?; }   // friends
```

Each re-raise uses the function's own `?`. So `From` conversions happen exactly as in the
original. `?` on an `Option` works the same way (the `option_question_mark` test).

`return v` is recorded in a slot that holds `v`. A `break` or `continue` that leaves the node is
recorded as an index, and a `match` re-performs it. Exits aimed at loops or labeled blocks
*inside* the node stay as they are.

## `return` in the middle

```rust,ignore
async fn early(stop: bool, api: &Api) -> Result<u32, Error> {
    let a = api.one().await;
    if stop {
        return Ok(0);
    }
    let b = api.two().await;
    Ok(a + b)
}
```

```text
  region 0:
    n0: a = one() [await]
    n1: if stop { return Ok (0) ?
    n2: b = two() [await]
    level 0: n0 || n1 || n2
```

All three run in one level. After the join:

```rust,ignore
if let Some(v) = __psync_r1_0 {
    return v;
}
```

The function still returns `Ok(0)` when `stop` is true. But **`api.two()` was called anyway**. This
is speculation. The sequential code would not have made that call.

## Speculation is the price of overlap

Overlapping an await with an earlier exit means starting it before you know whether you will need
it. psync accepts this for awaited bindings and await statements. That is the point of the
attribute. It does not accept it for ordinary statements: if an ordinary statement can exit, every
later ordinary statement waits for it (`effect order`).

For reads this is harmless. For writes it is not, so psync does not speculate the writes it
recognizes: an awaited `delete`, `put`, `upload`, `create`, a file write or a process waits for
every earlier statement that may exit. What you still assert by adding `#[psync::graph]` is that
the awaited calls psync could *not* classify tolerate running speculatively. `explain` lists them,
and `#[psync::graph(strict)]` treats them as writes instead.

These tools keep speculation away from code that must not speculate:

- Effects that psync recognizes as writes (file writes, service writes such as `delete` or
  `upload`, processes) wait for earlier exits. See
  [Side effects: files, network and processes](../safety/effects.md).
- `#[psync::graph(strict)]` treats unclassified awaited calls as writes.
- [`#[psync::seq]`](./hints.md) on a statement keeps it exactly in place.
- A lock, transaction or permit around the critical part is a barrier.

The opposite hint, `#[psync::speculate]`, lets a write run ahead anyway, for idempotent
operations where the extra work is harmless.

## Loops

In prefetched loops, each item's prefix records its own exits. The rest of the loop body replays
them item by item, in order (`loop_exits_are_replayed_per_iteration`):

```rust,ignore
for item in items {
    let Some(found) = io(T, item).await else {
        continue;
    };
    if found == 0 {
        continue;
    }
    out.push(found);
}
```

`let ... else { continue }` inside the prefix becomes "record a `continue`". The rest of the body
for that item performs it. Later items are unaffected.

A `?` or `return` in a prefix ends the whole loop at that item, as it would sequentially. The
prefixes of later items have already run by then. The same speculation applies, so a prefix that
writes is not prefetched in a loop that can end early, unless the loop is `#[psync::speculate]`.

## What psync cannot rewrite

Some exits cannot be recorded, so the statement holding them becomes a barrier:

- `?` or `return` inside a macro call (`my_macro!(x?)`), and the macros `bail!`, `ensure!`,
  `try!`, `return_err!` and `fail!`. Their tokens are opaque.
- `break` with a value.
- `yield`.

Logging macros (`trace!`, `debug!`, `info!`, `warn!`, `error!`, `log!`) are an exception. They use
`?x` and `%x` as field sigils, so a `?` in them is not an exit.
