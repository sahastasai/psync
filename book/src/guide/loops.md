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
- **All results are buffered.** The loop holds every item's prefix output (and creates every
  item's future up front) until the rest of the body runs. `bound` does not reduce this. For very
  long loops over large results, batch the input yourself
  ([Memory](./patterns.md#memory)).

`bound` defaults to 16. You set it per function: `#[psync::graph(bound = 4)]`.

## When a loop stays sequential

A prefix may only touch per-iteration data. That means the loop pattern and anything bound inside
the body. If it reads something an earlier iteration writes, the iterations depend on each other:

```rust,ignore
for item in items {
    let next = combine(total, item).await;    // reads `total`, which the previous iteration wrote
    total = next;
}
```

```text
  loop `for item in items`: reads `total`, which the loop body writes
```

The prefix may not have outside effects that later iterations would see either. A prefix that
writes the same file or service in every iteration, or writes anything at all when an earlier item
might end the loop, stays in order.

The reasons a loop is left alone are:

| Reason | Meaning |
|---|---|
| ``writes `x`, which outlives the iteration`` | the prefix writes loop-carried state |
| ``reads `x`, which the loop body writes`` | the prefix reads loop-carried state |
| `would <effect> in every iteration at once` | the prefix writes a file or service whose target does not depend on the item, so every iteration would write the same thing |
| `<effect> vs <effect>, which every iteration performs` | the prefix's effect conflicts with one the rest of the body has |
| `would <effect> for later items before knowing whether an earlier item ends the loop` | the body can `?`, `return` or `break`, and the prefix writes; `#[psync::speculate]` allows it (see [Fission](#fission-loop-carried-state-first)) |
| `no awaited statement can run ahead` | nothing awaited is left once the above are excluded |
| `` `ref` in the loop pattern `` | `ref` bindings cannot be moved into a closure |

`<effect>` is written as in [`explain`](./explain.md), for example ``write `store` `d` (upload)``.

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

As written, this loop stays sequential, for a reason that has nothing to do with the dedupe set:

```text
  loop `for d in digests`: would write `store` `d` (upload) for later items before knowing whether an earlier item ends the loop (`#[psync::speculate]` allows it)
```

`upload` writes, and its `?` can end the loop. If item 3's upload fails, sequential code never
uploads item 4, but a prefetching loop already has. Here that is harmless: uploads are
content-addressed, so uploading a blob nobody records is a no-op. Say so on the loop:

```rust,ignore
#[psync::speculate]
for d in digests { /* same body */ }
```

```text
  loop `for d in digests`: prefetch [__psync_g8 = upload()], 8 at a time
```

The generated code first builds `[(d, published.insert(d)) for each d]` in a plain loop. Then it
prefetches `upload` for the entries whose flag is true. This is jdx/mise's remote task cache. It is
the `tools-13` shape in `graph_suite`, with the same hint, at about 3.5×.

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

## Exercises

Predict the `loop` line of the report, then check with `psync-expand FILE LINE` on a file holding
the function.

1. Add a second lookup to `sizes`, after `meta`: `let tags = store.tags(key).await?;`, and add
   `tags.len() as u64` to `total`. What does psync prefetch, and how many at a time?
2. Instead, add `store.touch(key).await?;` after `meta`. What is prefetched now? Why? What would
   `#[psync::speculate]` on the loop change?
3. Predict the report for both of these. Why do they differ?

   ```rust,ignore
   async fn save_all(items: &[Item]) {
       for item in items {
           let bytes = item.json();
           tokio::fs::write("out.json", bytes).await;
       }
   }
   ```

   The second version writes `format!("{}.json", item.id)` instead of `"out.json"`.
4. This function passes the previous size to each lookup. What does psync say?

   ```rust,ignore
   async fn sizes(store: &Store, keys: &[String]) -> u64 {
       let mut total = 0;
       let mut last = 0;
       for key in keys {
           let meta = store.head(key, last).await;
           last = meta.size;
           total += meta.size;
       }
       total
   }
   ```

5. This loop returns at the first big object:

   ```rust,ignore
   async fn first_big(store: &Store, keys: &[String]) -> Option<u64> {
       for key in keys {
           let meta = store.head(key).await;
           if meta.size > 100 {
               return Some(meta.size);
           }
       }
       None
   }
   ```

   Is it rewritten? With 1000 keys, where the first one is big, how many `head` requests does
   the sequential loop make, and how many does the rewritten one make? When would that matter?

<details>
<summary>Answers</summary>

1. Both: ``prefetch [meta = head()?, tags = tags()?], 8 at a time``. The prefix is every leading
   awaited statement that touches only the item, and `bound = 8` sets how many run at once.
2. ``prefetch [meta = head()?], 16 at a time``. `touch` is one of the method names psync treats
   as a write to its receiver ([Side effects](../safety/effects.md)), and `head`'s `?` can end
   the loop. A prefetching loop would touch later keys before knowing whether an earlier key
   failed, which sequential code never does. With `#[psync::speculate]` it says
   ``prefetch [meta = head()?, touch()?]``: you have said that touching a key that sequential
   code would not reach is fine. (This function has no `bound`, so it is 16.)
3. The first stays sequential:
   ``would write file "out.json" (fs::write) in every iteration at once``. Every iteration
   writes the same file, and the last write must win. `#[psync::speculate]` does not change
   this, because it is about exits, not about order. The second is rewritten:
   ``prefetch [bytes = item . json (), fs::write()], 16 at a time; per-item files checked at run time``.
   The names differ per item, but psync cannot prove that two items never have the same `id`.
   It compares the paths before starting, and falls back to the sequential loop if two are the
   same ([Checking per-item files](../internals/loops.md)).
4. ``reads `last`, which the loop body writes``. Each lookup needs the previous iteration's
   result, so the iterations really are a chain, and the loop stays as written.
5. Yes: ``prefetch [meta = head()], 16 at a time``. The `return` is replayed in order, so the
   function returns the first key's size as before. But all 1000 `head` requests run, at most
   16 at a time, against 1 for the sequential loop. All prefixes complete before the rest of the
   body runs for the first item. That is fine when the big object is usually late, or not there.
   It is wasteful when it is usually early, and harmful against a rate-limited service. Then
   keep the loop sequential with `#[psync::seq]`, or split the keys into batches.

</details>
