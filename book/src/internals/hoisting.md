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

**Branches** are the same split that loops use for guards. The branch's leading awaited work runs
under the same condition, wrapped in `Option`. The rest of the branch unpacks it, re-raises any
recorded exit, and continues.

## If nothing overlaps, nothing changes

Hoisting is speculative. If the region still has no level with two chains afterwards, the rewriter
returns the block's original statements. So you never get `__psync_h` bindings in code that psync
did not speed up.
