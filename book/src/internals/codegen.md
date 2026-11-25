# Generated code

This chapter follows one function through code generation. The output below is real `psync-expand`
output, lightly trimmed.

```rust,ignore
async fn profile_page(api: &Api, user_id: u64) -> Result<Page, Error> {
    let user = api.user(user_id).await?;
    let posts = api.posts(user_id).await?;
    let avatar = api.avatar(&user.avatar_url).await?;
    let friends = api.friends(user_id).await?;
    Ok(Page { user, posts, avatar, friends })
}
```

## One chain

Each chain becomes an `async` block, labeled `'__psync_chainK`. Its exit slots are declared first:

```rust,ignore
let mut __psync_f0 = ::psync::__private::maybe_done_at::<1, 0, 2, true, 0, false, false, _>(async {
    let mut __psync_e0_0_0 = ::core::option::Option::None;
    let mut __psync_e0_1_0 = ::core::option::Option::None;
    #[allow(unused_labels, unreachable_code, clippy::let_unit_value)]
    let __psync_out = '__psync_chain0: {
        let user = match ::psync::__private::Branch::branch(api.user(user_id).await) {
            ::core::result::Result::Ok(v) => v,
            ::core::result::Result::Err(r) => {
                __psync_e0_0_0 = ::core::option::Option::Some(r);
                break '__psync_chain0 (::core::option::Option::None);
            }
        };
        let avatar = match ::psync::__private::Branch::branch(api.avatar(&user.avatar_url).await) {
            ::core::result::Result::Ok(v) => v,
            ::core::result::Result::Err(r) => {
                __psync_e0_1_0 = ::core::option::Option::Some(r);
                break '__psync_chain0 (::core::option::Option::None);
            }
        };
        ::core::option::Option::Some((user, avatar))
    };
    (__psync_e0_0_0, __psync_e0_1_0, __psync_out)
});
```

- The chain's value is `Some(bindings)` if it ran to the end, and `None` if an exit stopped it.
- Slot names encode the chain, the member and the `?` index: `__psync_e{chain}_{member}_{n}`.
  There is also `__psync_r…` for `return` values and `__psync_c…` for `break`/`continue`.
- Each `?` is rewritten by `exits::ExitRewriter`, innermost first, so `a.await?.b()?` records both.
  Closures, async blocks and nested items keep their own exits.

## The join

```rust,ignore
::core::future::poll_fn(|__psync_cx| {
    let __psync_done0 = __psync_f0.poll_step(__psync_cx);
    let __psync_done1 = __psync_f1.poll_step(__psync_cx);
    let __psync_done2 = __psync_f2.poll_step(__psync_cx);
    if true && __psync_done0 && __psync_done1 && __psync_done2 {
        ::core::task::Poll::Ready(())
    } else {
        ::core::task::Poll::Pending
    }
})
.await;
(__psync_f0.take(), __psync_f1.take(), __psync_f2.take())
```

Every poll of the join polls each unfinished chain once. Finished chains keep their output inside
`MaybeDone` until `take()`. Because the join is an ordinary `poll_fn`, it needs nothing from the
executor. It works on Tokio, embassy or psync's executor alike.

## Re-raising

```rust,ignore
let (__psync_e0_0_0, __psync_e0_1_0, __psync_out0) = __psync_taken0;
let (__psync_e1_0_0, __psync_out1) = __psync_taken1;
let (__psync_e2_0_0, __psync_out2) = __psync_taken2;
if let Some(r) = __psync_e0_0_0 { ::psync::__private::Rethrow::rethrow(r)?; }  // line 2: user
if let Some(r) = __psync_e1_0_0 { ::psync::__private::Rethrow::rethrow(r)?; }  // line 3: posts
if let Some(r) = __psync_e0_1_0 { ::psync::__private::Rethrow::rethrow(r)?; }  // line 4: avatar
if let Some(r) = __psync_e2_0_0 { ::psync::__private::Rethrow::rethrow(r)?; }  // line 5: friends
let (user, avatar) = __psync_out0.unwrap();
let (posts,) = __psync_out1.unwrap();
let (friends,) = __psync_out2.unwrap();
Ok(Page { user, posts, avatar, friends })
```

The re-raises are sorted by source position, not by chain, so `posts`'s error is checked before
`avatar`'s. The `unwrap()`s cannot fail: they run only when no exit was recorded, and a chain
without a recorded exit always ends in `Some`.

## `Branch` and `Rethrow`

`?` cannot be stored and replayed directly. psync splits it into two traits in `psync::__private`:

```rust,ignore
pub trait Branch {           // `?`-style split: the value, or a residual to rethrow later
    type Output;
    type Residual: Rethrow;
    fn branch(self) -> Result<Self::Output, Self::Residual>;
}
pub trait Rethrow {          // re-raise through the caller's own `?`
    type Carrier;
    fn rethrow(self) -> Self::Carrier;
}
```

`Result<T, E>` branches to `ResultResidual<E>`, which rethrows as `Err::<(), E>(e)`. `Option<T>`
branches to `OptionResidual`, which rethrows as `None::<()>`. Applying the caller's `?` to the
rethrown carrier runs the same `From` conversion the original `?` would have. So the function's
error type and conversion are unchanged.

## `return`, `break`, `continue`

- `return v` becomes `{ __psync_r = Some(v); break 'chain None }`. It is replayed as `return v;`.
- A `break` or `continue` that leaves the node is stored as an index into a list of targets. It is
  replayed by `match __psync_c { Some(0) => { continue 'outer; } ... }`.
- `break` with a value is not supported. It makes the statement a barrier.
