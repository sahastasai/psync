# Procedural macros

A **procedural macro** is a Rust function that the compiler calls while it compiles your code.
It receives tokens and returns tokens, and the compiler parses what it returns in place of what
you wrote. `#[psync::graph]` is an **attribute macro**, the kind that attaches to an item:

```rust,ignore
#[proc_macro_attribute]
pub fn graph(attr: TokenStream, item: TokenStream) -> TokenStream
//           ^^^^ the tokens inside #[psync::graph(...)]
//                               ^^^^ the whole `async fn` it is attached to
```

That is the entire interface. The macro cannot ask the compiler anything: what a name means,
what type an expression has, or what another function does. It sees the tokens of one item, and
it must decide from them what to emit.

Almost every macro has the same three steps:

```text
 tokens ──parse──► syntax tree ──analyse + transform──► syntax tree ──quote──► tokens
         syn::parse2                                                 quote!
```

## `quote!`: writing code that writes code

The [`quote`](https://docs.rs/quote) crate turns Rust-like text back into tokens. `#name`
interpolates a variable that holds tokens (an `Ident`, an `Expr`, a `TokenStream`, anything that
implements `ToTokens`). `#(#items),*` repeats once for each element of an iterator, with a comma
between:

```rust,ignore
let names = [user, posts, friends];             // Idents
let futures = [f_user, f_posts, f_friends];     // Exprs
let stmt = quote! {
    let (#(#names),*) = ::futures::join!(#(#futures),*);
};
// let (user, posts, friends) = ::futures::join!(api.user(user_id), api.posts(user_id), ...);
```

Generated code always names paths in full, as `::futures::join!` or `::core::option::Option`.
The user's crate might have its own `Option` in scope. The toy below uses `::futures::join!` for
brevity, which would require the user to depend on `futures`. psync's real generated code calls
only `::core` and its own runtime, through the path `#krate::__private`. The
user can change `krate` with `#[psync::graph(crate = path)]` when they re-export psync.

## psync in a hundred lines

`psync-graph/examples/mini_psync.rs` is a working toy version of `#[psync::graph]`. Its function
`mini_graph(item: TokenStream) -> syn::Result<TokenStream>` has the signature of an attribute
macro, and it is called on a token stream built with `quote!`. It accepts one shape only: a
block of `let name = <future>.await;` statements, then a final value.

```sh
cargo run -p psync-graph --example mini_psync
```

```text
analysis:
  edge s0 -> s2 (reads `user`)
  level 0: user || posts || friends
  level 1: avatar

generated:
async fn profile_page(api: &Api, user_id: u64) -> Page {
    const __MINI_GRAPH: () = ();
    let (user, posts, friends) = ::futures::join!(
        api.user(user_id), api.posts(user_id), api.friends(user_id)
    );
    let avatar = api.avatar(&user.avatar_url).await;
    Page {
        user,
        posts,
        avatar,
        friends,
    }
}

refused: mini_graph: the value must be `<future>.await` (no `?`)
```

Read the source next to this chapter. The three steps are marked in it:

1. **Analysis.** For each statement, keep the bound name, the future (the expression under
   `.await`), and every one-segment path the future mentions (its *reads*).
2. **The graph.** Draw an edge `j → i` when statement `i` reads the name statement `j` binds.
   Put each statement on a *level* one past the deepest level it reads from.
3. **Code generation.** Emit one statement per level: the original `let` if the level has one
   future, and a `join!` of all of them if it has more.

The output is correct for this input. It is also already worse than psync. `avatar` waits for
`posts` and `friends` as well, because they share a level. psync would run `user → avatar` as a
*chain* alongside the other two. [Dependencies and dataflow](./dataflow.md) explains chains.

## Refusing is a feature

When `mini_graph` meets something it does not understand, it returns a `syn::Error` built with
`new_spanned(tokens, message)`. A real macro turns that into code with `to_compile_error()`, which
expands to `compile_error!("...")` carrying the span of those tokens. The user then sees an
ordinary compiler error pointing at their own statement.

psync rarely refuses. When it cannot rewrite a statement, it leaves the statement alone. A
statement psync cannot move becomes a **barrier**, and a region with nothing to overlap is
emitted *exactly as written*. Its worst case is your original code. The toy refuses `?` because
handling it correctly is subtle. The exercise at the end of this chapter shows why.

## Hygiene and generated names

If a macro introduces a variable `tmp`, and the user's code also has a `tmp`, which one does
`tmp` refer to? A **hygienic** macro system keeps them apart automatically. Rust's
`macro_rules!` is partly hygienic for local variables. Procedural macros choose per token, via
spans:

- `Span::call_site()`: the token behaves as if the user had written it. It can see the user's
  variables, and they can see it.
- `Span::mixed_site()`: local variables are private to the macro, like `macro_rules!`.

psync *needs* call-site behaviour. The code it moves into chains must still refer to the user's
`api` and `user_id`. So psync prefixes everything it invents with `__psync_`
(`__psync_f0`, `__psync_e0_1_0`, `'__psync_chain0`) and counts names up within the function so
they never collide.

## What psync actually generates

`psync-expand FILE LINE` runs the real analysis on the function containing `LINE` and prints the
graph and the generated code as JSON. Here it is on the introduction's example, with `?` this
time:

```sh
cargo run -p psync-graph --features cli --bin psync-expand -- page.rs 2
```

Shortened, and with the paths trimmed, this is the code that comes out:

```rust,ignore
async fn profile_page(api: &Api, user_id: u64) -> Result<Page, Error> {
    const __PSYNC_GRAPH: Option<&str> = option_env!("PSYNC_FACTS_STAMP");
    let (__psync_taken0, __psync_taken1, __psync_taken2) = {
        // Chain 0: user -> avatar. Join 1 of this function, starts at statement 0, may exit.
        let mut __psync_f0 = __private::maybe_done_at::<1, 0, true, false, false, _>(async {
            let mut __psync_e0_0_0 = None;                // where `user`'s `?` is recorded
            let mut __psync_e0_1_0 = None;                // where `avatar`'s `?` is recorded
            let __psync_out = '__psync_chain0: {
                let user = match Branch::branch(api.user(user_id).await) {
                    Ok(v) => v,
                    Err(r) => {
                        __psync_e0_0_0 = Some(r);         // record the exit...
                        break '__psync_chain0 None;       // ...and stop this chain only
                    }
                };
                let avatar = /* the same, into __psync_e0_1_0 */;
                Some((user, avatar))
            };
            (__psync_e0_0_0, __psync_e0_1_0, __psync_out)
        });
        let mut __psync_f1 = /* chain 1: posts, starts at statement 1 */;
        let mut __psync_f2 = /* chain 2: friends, starts at statement 3 */;
        // Poll every chain until all are done: the join.
        poll_fn(|cx| {
            let d0 = __psync_f0.poll_step(cx);
            let d1 = __psync_f1.poll_step(cx);
            let d2 = __psync_f2.poll_step(cx);
            if d0 && d1 && d2 { Poll::Ready(()) } else { Poll::Pending }
        }).await;
        (__psync_f0.take(), __psync_f1.take(), __psync_f2.take())
    };
    // Each chain returned (its exit slots..., Some(bindings) or None): unpack them.
    let (__psync_e0_0_0, __psync_e0_1_0, __psync_out0) = __psync_taken0;
    let (__psync_e1_0_0, __psync_out1) = __psync_taken1;
    let (__psync_e2_0_0, __psync_out2) = __psync_taken2;
    // Replay the exits in PROGRAM order:
    if let Some(r) = __psync_e0_0_0 { Rethrow::rethrow(r)?; }   // user    (statement 0)
    if let Some(r) = __psync_e1_0_0 { Rethrow::rethrow(r)?; }   // posts   (statement 1)
    if let Some(r) = __psync_e0_1_0 { Rethrow::rethrow(r)?; }   // avatar  (statement 2)
    if let Some(r) = __psync_e2_0_0 { Rethrow::rethrow(r)?; }   // friends (statement 3)
    let (user, avatar) = __psync_out0.unwrap();
    let (posts,) = __psync_out1.unwrap();
    let (friends,) = __psync_out2.unwrap();
    Ok(Page { user, posts, avatar, friends })
}
```

The runtime pieces are small. `maybe_done_at(future)` boxes a chain's future. `poll_step(cx)`
polls it if it is still running and returns `true` once it has finished. `take()` hands out its
output. Every part of this has a chapter in [How psync works](../internals/overview.md). Three
things matter here:

- **Exits are data until the end.** A `?` inside a chain does not leave the function. It stores
  the error in a slot and ends only its own chain. After the join, the slots are checked in the
  order the statements were written, so the function fails at the same `?`, with the same error,
  as the sequential code would. This is what `exits.rs` does.
- **The macro leaves notes for the compiler.** `maybe_done_at::<1, 0, true, false, false, _>` does
  nothing at run time beyond `maybe_done`. Its const generic arguments record which join of the
  function the chain belongs to (they are numbered from 1), the statement the chain starts at,
  whether it may exit, and the `speculate` and `independent` hints. Generic arguments survive all the way into MIR, so `psync-driver` can read back exactly
  what the macro assumed, and check it. The constant `__PSYNC_GRAPH` marks the function as
  rewritten. Its value, `option_env!("PSYNC_FACTS_STAMP")`, makes cargo rebuild the crate when
  the compiler facts change.
- **rustc checks the result.** Everything above is ordinary Rust. If psync had moved two uses of
  one `&mut` into concurrent chains, this code would fail the borrow checker, so the mistake would
  show up as a compile error, never as a data race.

## Debugging a macro

- `psync-expand` shows psync's output for any function, annotated or not.
- `#[psync::graph(explain)]` prints the graph while your crate compiles.
- `cargo expand` (a separate tool) prints any crate after all macros have run.
- In tests, call the library directly. `psync-graph`'s unit tests call `transform_fn` on
  functions built with `syn::parse_quote!` and assert on the report.

## Exercises

1. `mini_psync` ignores shadowing: a statement's level depends only on what it reads. Is that
   correct? Consider a statement 3 that is `let user = ..` again.
2. Why does `mini_graph` refuse `?`? Suppose it emitted, for each level, the join and then the
   `?`s of that level in program order. Find a program where this returns a *different error*
   from the sequential code. (Hint: use three statements, where the second depends on the first
   and the third depends on nothing.)
3. In the real output, why is `avatar`'s exit replayed *between* `posts`'s and `friends`'s, when
   `avatar` belongs to chain 0?
4. Why does generated code write `::core::option::Option::None` instead of `None`?

<details>
<summary>Answers</summary>

1. It is *not* correct to ignore shadowing. If statement 3 rebinds `user`, then the final
   `Page { user, .. }` must see statement 3's value. With both at level 0, the tuple pattern
   `let (user, .., user) = ..` would not even compile. psync adds a "rebinds" edge (both bind the
   name) and a "shadows" edge (a later statement rebinds a name an earlier one still reads).
2. Take `let a = f().await?; let b = g(a).await?; let c = h().await?;`. Levels: `a` and `c` at 0, `b`
   at 1. Suppose `a` succeeds, `b` fails and `c` fails. Sequentially, `b`'s error is returned and
   `h` never runs. The toy would run `a || c`, replay level 0's exits, and return `c`'s error,
   which is wrong. Exits must be replayed in program order across *all* the work that ran. psync
   achieves this by ending only the failing chain and replaying all exits after the join, and by
   keeping a write from starting before an earlier exit.
3. Replay follows statement order, not chain order: `user` (0), `posts` (1), `avatar` (2),
   `friends` (3). If `posts` and `avatar` both fail, sequential code returns `posts`'s error first.
4. The user may have shadowed `None` or `Option`: a local enum, an `use` of something else, or a
   `#![no_implicit_prelude]` crate. A full path from the crate root `::core` cannot be shadowed.

</details>
