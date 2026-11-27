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
    local: Set,                   // the loop pattern and everything bound inside the body
    carried: Set,                 // places the body writes that outlive an iteration
    independent: bool,            // #[psync::independent]
    terminates: bool,             // the body may end the loop (`?`, `return`, `break`)
    speculate: bool,              // #[psync::speculate] on the loop or the function
    carried_effects: Vec<Effect>, // side effects whose target is the same in every iteration
}
```

`LoopScope::refuses` refuses a candidate prefix node, in this order, when:

1. the loop `terminates`, the node writes something outside the program, and neither the loop nor
   the node is marked `speculate`. Later items' writes would happen before an earlier item ends
   the loop;
2. it writes a file or service whose target does not mention a per-iteration name, so every
   iteration writes the same thing;
3. its side effects conflict with `carried_effects`;
4. it writes a non-local path;
5. it reads a path in `carried`.

`independent` skips checks 2 to 5, but not 1: knowing that iterations do not depend on each other
says nothing about work done after the loop should have ended.

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
let __psync_each = ::psync::__private::over_at::<2, true, false, false, _, _, _>(&__psync_iter, async |<pat>| { <prefix chain> });
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

## Checking per-item files

Prefetching runs item 3's prefix before item 2's rest. That is safe when each item touches its own
files, which a list with duplicates breaks:

```rust,ignore
for name in names {
    let path = dir.join(name);
    let old: u32 = fs::read_to_string(&path).await?.parse()?;
    std::fs::write(&path, (old + 1).to_string())?;
}
```

Run ahead over `["a", "b", "a"]`, both reads of `a` see `0`, and `a` ends at `1` instead of `2`.
So when a file the loop touches is computed from the item, and the prefix touches one of them,
and one is written, the macro evaluates those paths for every item before it starts any:

```rust,ignore
let __psync_iter: Vec<_> = IntoIterator::into_iter(<iter>).collect();
let __psync_distinct: bool = {
    let mut __psync_seen = HashMap::<PathBuf, bool>::new();   // path -> written
    __psync_iter.iter().all(|__psync_r| {
        let <pat> = __psync_r;                                // the item's names, by reference
        // each path, e.g. `&(dir.join(name))`, normalized; then: no path that another item
        // also touches, where either writes it
    })
};
if __psync_distinct { <the prefetched loop> } else { <the loop as written> }
```

The paths that count are the prefix's (reads and writes) and the rest's writes. Two reads of one
file do not need ordering. A path is compared after `Path::components`, which drops `.` and
repeated separators, so `dir/./a` and `dir/a` are one file. Symlinks, hard links, case-folding
file systems and a relative path against an absolute one can still make two different strings
name one file. That is the same assumption psync makes for two different literals.

A path is evaluated ahead only when that is harmless:
- it is **total**: names, fields, references, `format!`, `join`, `with_extension`, `to_path_buf`
  and the like, and `Path::new`/`PathBuf::from`. There is no indexing, no arithmetic, and no other
  call, because evaluating it for an item the loop never reaches must not panic;
- it depends only on the **loop pattern**, or on a `let` in the body bound once to a total value
  (`let path = dir.join(name)`), which is substituted;
- the pattern can bind **by reference**, which `mut` inside a tuple pattern prevents.

When one of these fails, as for `let path = api.locate(name).await?`, distinct items are
assumed, as before. The report says which: `psync-expand` prints `per-item files checked at run
time`, or `per-item files assumed distinct: the path ... depends on `path`, computed in the
body`. `#[psync::independent]` on the loop skips the check. Only files are checked. A per-item
target in another domain, such as a service object or an environment variable, is still assumed
to differ between items.

`tests/targets.rs` runs both cases. Without the check, a file named twice ends at `1`.

### Exercises

Each loop is `for name in names { .. }` in an async function with `dir: &Path`,
`dirs: &[PathBuf]` and `api: &Api`. Predict the `loop` line of the report, then check with
`psync-expand`.

1. ```rust,ignore
   let path = dir.join(name);
   let old = tokio::fs::read_to_string(&path).await;
   tokio::fs::write(&path, old).await;
   ```
2. The same, with `let path = api.locate(name).await;`.
3. The same, with `let path = dirs[0].join(name);`.
4. ```rust,ignore
   let old = tokio::fs::read_to_string(dir.join(name)).await;
   tokio::fs::write(dir.join("total"), old).await;
   ```

<details>
<summary>Answers</summary>

1. ``prefetch [..], 16 at a time; per-item files checked at run time``. The path is total and
   depends only on the item, so the generated code compares every item's path first, and runs
   the loop as written if two are the same file.
2. ``per-item files assumed distinct: the path `& path` depends on `path`, computed in the body``.
   An awaited value cannot be computed ahead for every item, so psync falls back to assuming
   that different items name different files.
3. The same message. `dirs[0]` is indexing, which may panic, so the `let` is not total, and `path`
   is not substituted. Evaluating it for an item the loop never reaches must not panic.
4. Not prefetched: ``write file `dir . join ("total")` (fs::write) vs read file `dir . join (name)` (fs::read_to_string), which every iteration performs``.
   Every iteration writes the same file, which may be one that another iteration's prefix reads
   (`name` could be `"total"`).

</details>

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
