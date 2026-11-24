# Loop rewriting

`loops.rs` handles `for` loops (always considered) and `while` loops (only with
`#[psync::independent]`). For a `for` loop, `rewrite_loop` does three steps:

1. **Flatten** a directly nested `for` into one loop over pairs, when that is the same loop.
2. Try **prefetch**.
3. If prefetch is refused, try **fission**, then prefetch the second half.

Either way it records a `LoopReport`.

## The loop scope

```rust,ignore
struct LoopScope {
    local: Set,        // the loop pattern and everything bound inside the body
    carried: Set,      // places the body writes that outlive an iteration
    independent: bool, // #[psync::independent]
}
```

A candidate prefix node is refused when it writes a non-local path, or reads a path in `carried`.
`independent` skips both checks.

## Taking the prefix

`take_prefix` walks the body from the front:

- Bindings and awaits are taken while they pass the scope check. They are classified in relaxed
  mode, so non-pure per-iteration bindings may join.
- Further on, an awaited statement may still join if nothing left behind before it could exit the
  iteration, or touches what it touches.
- Awaits in scrutinees, call arguments and `if` branches are hoisted first (the same splits as
  [Exposing hidden awaits](./hoisting.md)).
- `if c { <awaited work>; rest }` becomes a guard binding, `hoist_guard`.
- `if c { ..; continue }` with a pure `c`, followed by awaited work, becomes an inverted guard,
  `invert_guard`: `let g = if !(c) { Some(<work>) } else { None };`. The rest of the body then
  unpacks `g` with `let Some(..) = g else { unreachable!(..) }` after the original guard. When the
  guard did not exit, its condition was false, so `g` is `Some`.

## Prefetch

The prefix becomes one chain (see [Generated code](./codegen.md)). The pattern's bindings and the
prefix's bindings are handed to the rest of the body:

```rust,ignore
let __psync_iter = <iter>;
let __psync_each = ::psync::__private::over(&__psync_iter, async |<pat>| { <prefix chain> });
let __psync_futures: ::psync::__private::Vec<_> = IntoIterator::into_iter(__psync_iter)
    .map(|__psync_item| __psync_each(__psync_item))
    .collect();
let __psync_items = ::psync::__private::join_all_bounded(__psync_futures.into_iter(), <bound>).await;
<label> for (<exit slots>, __psync_out) in __psync_items {
    <re-raise this item's exits>
    let <bindings> = __psync_out.unwrap();
    <rest of the body>
}
```

Two parts of this shape exist because of real compile failures:

- **`over`** is an identity function with a bound, `F: AsyncFn(I::Item) -> T`. A closure defined on
  its own cannot have its parameter type inferred from later use. Passing it through `over` gives the
  parameter the iterator's item type.
- **The futures are collected before the await**, in the function itself, not inside a generic
  helper. rustc can then prove the state held across the await is `Send`, even when items carry
  lifetimes (`Box<dyn Trait + Send>`). It cannot prove that for an iterator adapter or a helper's
  future. This came up in DataFusion's writer shutdown.

Futures are lazy, so collecting them starts nothing. `join_all_bounded` starts at most `bound` at a
time.

## Fission

`fission_with(head, label, attrs, pattern, body)`:

1. **First half:** every statement before the first await. A leading `if`/`match` whose branches
   await contributes only its condition, hoisted with `hoist::condition`. Any blocker refuses fission.
2. **Check:** the two halves must not touch the same carried state, unless the loop is
   `independent`. Per-iteration names may flow from the first half to the second.
3. **Carry:** the first half's bindings that the second half mentions, plus the pattern's bindings.
4. **Generate:**

   ```rust,ignore
   let __psync_first = {
       let mut __psync_v = ::std::vec::Vec::new();
       '__psync_pre: <head> {
           <first half, with loop-ending exits recorded>
           __psync_v.push((<carried names>,));
       }
       __psync_v
   };
   <prefetched: for (<carried names>,) in __psync_first { <second half> }>
   <re-raise the first half's recorded exit>
   ```

The first half's exits are rewritten in *deferring* mode. An exit that ends the whole loop
(`return`, `?`, `break` to an outer loop) stops the first loop with a plain `break` and is recorded.
`continue` and `break` aimed at the loop itself stay as written. The second loop processes exactly
the items before the exit. The exit is then performed, as sequential code would after those items.

`while` loops use the same code with `head = while <cond>`. That is why they need `independent`: the
condition must be driven only by the first half, and tokens cannot show that.

## Flattening

```rust,ignore
for a in xs { for b in f(a) { body } }
// becomes
for (a, b) in IntoIterator::into_iter(xs).flat_map(move |a| {
    IntoIterator::into_iter(f(a)).map(move |__psync_inner| (a, __psync_inner))
}) { body }
```

This applies only when the outer body is exactly the inner loop, neither loop is labeled, `body`
has no `break` (which would end only the inner loop), and the outer pattern is a plain identifier.
`a` is copied into every pair, so it must be `Copy`. If it is not, the code fails to compile rather
than changing meaning.
