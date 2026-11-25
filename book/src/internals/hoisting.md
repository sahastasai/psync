# Exposing hidden awaits

Many awaits sit inside a larger statement:

```rust,ignore
lens.push(api.len(&names[0]).await);
match api.kind(&names[1]).await { /* ... */ }
let v = if cached { x } else { fetch().await? };
```

The await is evaluated before the rest of its statement anyway. If psync moves it into its own
binding, the order stays the same, and the await becomes a node that can overlap other work. That
is `hoist.rs`. `Rewriter::expose_awaits` applies it to every statement that hides an await,
repeatedly, up to a depth of 8, because a hoisted value may itself hide another await.

## The splits

| Split | Before | After |
|---|---|---|
| **scrutinee** | `match e.await {..}`, `if let p = e.await {..}`, `if e.await {..}`, `let p = e.await else {..}` | `let __psync_sN = e.await;` + the statement reading `__psync_sN` |
| **operands** | awaited arguments of a call or method call, the right side of `=`/`+=`, elements of an array, tuple or `vec!` | `let __psync_hN = <operand>;` for each operand up to the last awaited one |
| **receiver** | `x.await?.items()` | `let __psync_hN = x.await?;` + `__psync_hN.items()` |
| **branch** | `if c { let x = f().await?; rest }` | `let __psync_gN = if c { Some(<awaited prefix>) } else { None };` + the remainder |
| **tail** | an awaited block value `foo().await?` | `let __psync_tN = foo().await?; __psync_tN` |

**Operands keep their order.** When an awaited argument is hoisted, every earlier argument that is
not pure is hoisted too, in order. So `f(g(), h().await)` still evaluates `g()` before `h()`. Pure
arguments, which cannot observe the order, stay in place. Operands are only hoisted from calls with
side-effect-free callees: a method call whose receiver is a plain place, or a call to a path. For
`x += y`, the left side must be a place, so evaluating `y` first is unobservable.

**Bindings are split too.** In `let total = combine(a, api.extra().await);` the await is in the
binding's value, not the value itself, so the operand split applies: `extra()` becomes its own
binding and need not wait for `a`, which only `combine` reads. The values that hoisting creates
are split again, so `out.push((api.size().await, api.extra().await))` exposes both awaits. A
binding whose value is itself awaited (`let x = f().await?;`, `let n = f().await.len();`) is
already a node of its own. (Until this was fixed, only expression statements were split, and the
operands of a `let` waited for each other and for everything they followed. Test
`awaits_inside_bindings_overlap`.) The operands of a binary operator, as in
`api.size().await + api.extra().await`, are not split: bind them yourself.

**Branches** are the same split that loops use for guards. The branch's leading awaited work runs
under the same condition, wrapped in `Option`. The rest of the branch unpacks it, re-raises any
recorded exit, and continues.

## If nothing overlaps, nothing changes

Hoisting is speculative. If the region still has no level with two chains afterwards, the rewriter
returns the block's original statements. So you never get `__psync_h` bindings in code that psync
did not speed up.

## Exercises

Predict the nodes and levels, then check with `psync-expand FILE LINE`.

1. ```rust,ignore
   async fn f(api: &Api, log: &mut Vec<u32>) -> u32 {
       let a = api.one().await;
       let total = combine(log.pop().unwrap(), api.size().await);
       a + total
   }
   ```
   Why is `log.pop().unwrap()` hoisted, although it does not await?
2. ```rust,ignore
   async fn f(api: &Api) -> u32 {
       let a = api.one().await;
       let total = api.size().await + api.extra().await;
       a + total
   }
   ```
   Which calls overlap? How would you make all three overlap?
3. ```rust,ignore
   async fn h(api: &Api) -> u32 {
       let a = api.one().await;
       api.two().await
   }
   ```
   The last line is the block's value, a barrier. How do `one` and `two` still overlap?

<details>
<summary>Answers</summary>

1. Three nodes in level 0, `a = one()`, `__psync_h2 = log . pop () . unwrap ()` and
   `__psync_h3 = size()`, and then `total = combine(..)` in level 1. `pop` is hoisted because it
   comes before the awaited operand and is not pure. Hoisting only the await would run
   `size()` before `pop()`, against the order of evaluation. Chains of a level are polled in
   program order, so `pop` still runs before `size` starts.
2. `one` and the pair: `total = size()` is one node, `level 0: n0 || n1`, and inside it `extra`
   still waits for `size`, because a binary operator's operands are not split. Write
   `let s = api.size().await; let e = api.extra().await; let total = s + e;`, and all three
   start together.
3. The tail split: `let __psync_t1 = api.two().await; __psync_t1`. The await moves into a
   binding that can overlap `one`, and only the plain value `__psync_t1` stays last.

</details>
