# Loops

The most common missed concurrency in real code is an await inside a loop. In the audit behind psync,
it was 99 of 193 findings:

```rust,ignore
for key in keys {
    let meta = store.head(key).await?;   // one round trip per key, one at a time
    total += meta.size;
}
```

psync rewrites `for` loops so that the awaited work at the front of each iteration runs *ahead*,
up to `bound` iterations at a time. The rest of each body still runs in order.

## Prefetching

```rust,ignore
#[psync::graph(bound = 8)]
async fn sizes(store: &Store, keys: &[String]) -> Result<u64, Error> {
    let mut total = 0;
    for key in keys {
        let meta = store.head(key).await?;
        total += meta.size;
    }
    Ok(total)
}
```

```text
  loop `for key in keys`: prefetch [meta = head()?], 8 at a time
```

The leading awaited statements of the body are its **prefix**. Here that is
`let meta = store.head(key).await?`. psync turns the prefix into an async closure, runs it for
every item with at most 8 in flight, and then runs the rest of the body for each item, in order.
This is the real generated code, shortened:

```rust,ignore
let __psync_iter = keys;
let __psync_each = ::psync::__private::over_at::<2, true, false, false, _, _, _>(&__psync_iter, async |key| {
    // the prefix, with `?` recorded instead of taken
    let meta = match Branch::branch(store.head(key).await) { Ok(v) => v, Err(r) => { /* record r */ } };
    Some((meta,))
});
let __psync_futures: Vec<_> = __psync_iter.into_iter().map(|item| __psync_each(item)).collect();
let __psync_items = ::psync::__private::join_all_bounded(__psync_futures.into_iter(), 8usize).await;
for (__psync_e, __psync_out) in __psync_items {
    if let Some(r) = __psync_e { Rethrow::rethrow(r)?; }   // the first failing key, in order
    let (meta,) = __psync_out.unwrap();
    total += meta.size;
}
```

Three things to know about this shape:

- **`bound` limits how many prefixes are in flight, not how far ahead they run.** Each item's
  prefix starts as soon as a slot frees. All prefixes complete before the rest of the body runs for
  the first item.
- **Errors are replayed in order.** If key 3 and key 7 both fail, the function returns key 3's
  error, as the sequential loop would. But keys 4 to 7 (and every later key) were still looked up.
  That is the speculation the attribute allows. For reads it does not matter. For writes, see
  [Side effects](../safety/effects.md).
- **All results are buffered.** The loop holds every item's prefix output until the rest of the
  body runs. For very long loops, choose a `bound` and think about memory.

`bound` defaults to 16. You set it per function: `#[psync::graph(bound = 4)]`.

## When a loop stays sequential

A prefix may only touch per-iteration data. That means the loop pattern and anything bound inside
the body. If it reads something an earlier iteration writes, the iterations depend on each other:

```rust,ignore
for item in items {
    let next = add(total, item).await;    // reads `total`, which the previous iteration wrote
    total = next;
}
```

```text
  loop `for item in items`: no awaited statement can run ahead
```

The reasons a loop is left alone are:

| Reason | Meaning |
|---|---|
| ``writes `x`, which outlives the iteration`` | the prefix writes loop-carried state |
| ``reads `x`, which the loop body writes`` | the prefix reads loop-carried state |
| `no awaited statement can run ahead` | nothing awaited is left once the above are excluded |
| `` `ref` in the loop pattern `` | `ref` bindings cannot be moved into a closure |

Read-modify-write caches are a common case:

```rust,ignore
for k in keys {
    if !cache.contains_key(&k) {
        let v = api.get(k).await;
        cache.insert(k, v);
    }
}
```

psync cannot know whether two keys are equal. Two iterations with the same key would do the lookup
twice instead of once. So this loop stays sequential. If you know the keys are distinct, or a
duplicate lookup is fine, say so with [`#[psync::independent]`](./hints.md):

```rust,ignore
#[psync::independent]
for k in keys { /* same body */ }
```

## Guards

Work behind an `if` runs ahead only for the items that reach it:

```rust,ignore
for g in all {
    let key = g * 10;
    if self.mode.is_queryable(g) {
        let versions = self.client.query(g).await?;
        self.groups.push((key, versions));
    }
}
```

The prefix becomes `let __psync_g = if <cond> { Some(<awaited work>) } else { None };`. The rest of
the body unpacks it. Reads and writes are tracked per field, so writing `self.groups` does not
conflict with reading `self.mode` or `self.client`. The test
`guarded_loop_bodies_prefetch_with_field_sensitivity` checks this shape. It is kube-rs's discovery
loop.

An **early-exit guard** works too. Here a pure condition comes first and leaves the iteration:

```rust,ignore
for split in splits {
    if split == 0 {
        continue;
    }
    let m = api.search(split).await?;
    out.push(m);
}
```

```text
  loop `for split in splits`: prefetch [__psync_g2 = search()], 16 at a time
```

psync inverts the guard: the prefix is `if !(split == 0) { Some(search) } else { None }`. So no
search runs for an item that the guard skips. The `continue` is still performed in place.

## Fission: loop-carried state first

Some loops update shared state before they await: a dedupe set, a map removal, a reservation
counter. The awaited part depends on that state, but only on the current item's share of it.
psync splits the loop in two. First it runs the leading non-awaiting statements for every item. Then
it prefetches the rest:

```rust,ignore
for d in digests {
    if published.insert(d) {                 // carried state: runs first, for every item
        let receipt = store.upload(d).await?;
        uploaded.push(receipt);
    }
}
```

```text
  loop `for d in digests`: prefetch [__psync_g8 = upload()], 8 at a time
```

The generated code first builds `[(d, published.insert(d)) for each d]` in a plain loop. Then it
prefetches `upload` for the entries whose flag is true. This is jdx/mise's remote task cache. It is
the `tools-13` shape in `graph_suite`, at 3.46×.

Exits in the first loop that would end the whole loop are deferred until the items before them are
done. These are `return`, `?`, or a `break` to an outer loop. Sequential code performs them at that
point, too:

```rust,ignore
for table in tables {
    let Some(batch) = pending.remove(&table) else { continue };
    if batch == 0 {
        return Err(Error("empty batch"));    // first loop stops here; items before it are written,
    }                                         // then the error is returned
    written += w.write_batch(batch).await;
}
```

Fission only applies when the two halves do not touch the same carried state. Per-iteration names
flow from the first half to the second.

## Nested loops

A loop whose body is only another loop is treated as one loop over the pairs:

```rust,ignore
for group in &groups {
    for resource in group {
        if *resource == 0 { continue; }
        let items = api.list(*resource).await?;
        listed.push(items);
    }
}
```

```text
  loop `for group in & groups`: prefetch [__psync_g2 = list()], 16 at a time
```

All resources of all groups run ahead together, not just one group's. This only happens when:

- the outer body is exactly the inner loop;
- neither loop is labeled;
- the inner body has no `break`;
- the outer item is a plain binding.

The outer item is paired with every inner item, so it must be `Copy`, for example a reference. If it
is not, the generated code does not compile. It does not silently change meaning.

When the inner loop is not the whole body, the two loops are rewritten separately, and **their
bounds multiply**. An outer loop with bound 16 around an inner loop with bound 8 can have
16 × 8 = 128 requests in flight. [Patterns and pitfalls](./patterns.md) shows what that did to a
single storage server.

## `while` loops

psync cannot reason about a `while` condition from tokens. It cannot tell that the condition is
driven only by the statements before the first await. So `while` loops are rewritten only when you
assert it:

```rust,ignore
#[psync::independent]
while size < min {
    let guard = size as u32;
    size += 1;
    idle.push(connect(guard).await);
}
```

```text
  loop `while size < min`: prefetch [__psync_h4 = connect()], 16 at a time
```

The first half (`guard`, `size += 1`) runs as a plain `while` loop and collects each iteration's
`guard`. The connections are then made concurrently. This is sqlx's `try_min_connections`: reserve
in order, connect concurrently.
