# Syntax trees

Tokens say which characters belong together. They do not say how the pieces relate: that
`api.user(id)` is a method call whose receiver is `api`, or that `.await` applies to the whole
call and `?` applies to the awaited result. Working that out is **parsing**, and its result is a
**syntax tree**.

```text
tokens:   let user = api . user ( id ) . await ? ;

tree:     Local (a `let` statement)
          ├── pattern: user
          └── init:    Try                       the `?`
                       └── Await                 the `.await`
                           └── MethodCall .user
                               ├── receiver: Path api
                               └── args:     [Path id]
```

The tree makes evaluation order visible: inner nodes run first. The call runs, then its future
is awaited, then `?` inspects the result. The same tokens could not be read any other way.
Rust's **grammar** fixes exactly one tree for every valid program.

rustc's own syntax tree is called the **AST** (abstract syntax tree). It is "abstract" because it
drops what only mattered for reading: the parentheses in `(a + b) * c` become the shape of the
tree, and the commas between arguments disappear.

## `syn`: Rust's syntax tree as a library

Procedural macros cannot use rustc's AST, which is internal to the compiler. They use
[`syn`](https://docs.rs/syn), a parser for Rust that runs on a `TokenStream` and returns a tree of
ordinary Rust types. The types follow the grammar:

| `syn` type | Is | Examples |
|---|---|---|
| `File`, `Item` | a whole file; one item in it | `fn`, `struct`, `impl`, `use` |
| `ItemFn` | a function: attributes, signature, body | `async fn f(x: u32) -> u32 { .. }` |
| `Block` | `{ .. }`: a list of statements | |
| `Stmt` | one statement | `Stmt::Local` (a `let`), `Stmt::Expr`, `Stmt::Item`, `Stmt::Macro` |
| `Expr` | an expression, about 40 variants | `Expr::Await`, `Expr::Try`, `Expr::MethodCall`, `Expr::If`, `Expr::ForLoop` |
| `Pat` | a pattern | `user`, `(a, b)`, `Some(x)`, `Page { user, .. }` |

A `Stmt::Expr` carries an `Option<Token![;]>`. A block's last expression with no semicolon is the
block's value. psync cares about that difference. The value expression must stay last, so it is
always a *barrier*, psync's word for a statement that never moves.

Run the example:

```sh
cargo run -p psync-graph --example syntax_tree
```

```text
fn profile_page (5 statements, async: true)

s0: Try(Await(MethodCall(.user, receiver: Path(api), args: [Path(user_id)])))
    binds {"user"}  reads ["api", "user_id"]  awaits 1
s1: Try(Await(MethodCall(.posts, receiver: Path(api), args: [Path(user_id)])))
    binds {"posts"}  reads ["api", "user_id"]  awaits 1
s2: Try(Await(MethodCall(.avatar, receiver: Path(api), args: [Ref(Field(Path(user), avatar_url))])))
    binds {"avatar"}  reads ["api", "user"]  awaits 1
s3: Try(Await(MethodCall(.friends, receiver: Path(api), args: [Path(user_id)])))
    binds {"friends"}  reads ["api", "user_id"]  awaits 1
s4: Call(Path(Ok), [Struct(Page)])
    binds {}  reads ["Ok", "avatar", "friends", "posts", "user"]  awaits 0
```

The program (`psync-graph/examples/syntax_tree.rs`) does two things. `shape` is a recursive
`match` on `Expr` that prints each node's variant and children. `Access` walks each statement
with a **visitor** and records what it binds, what it reads and how many awaits it contains.
That record is the start of a dependency analysis. `s2` reads `user`, which `s0` binds, so `s2`
must wait for `s0`. Nothing else depends on anything.

## Visitors

A syntax tree for real code has hundreds of node types. Matching all of them by hand to find,
say, every `.await` would be long and fragile. `syn` generates a **visitor** trait instead,
`syn::visit::Visit`, with one method per node type. Each default method just visits the node's
children. You override the methods you care about and call the default to keep descending:

```rust,ignore
impl<'ast> Visit<'ast> for Access {
    fn visit_expr_await(&mut self, a: &'ast syn::ExprAwait) {
        self.awaits += 1;
        visit::visit_expr_await(self, a);     // keep walking into the awaited expression
    }
    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        if let Some(name) = p.path.get_ident() {
            self.reads.insert(name.to_string());
        }
        visit::visit_expr_path(self, p);
    }
}
```

There are three flavours:

- `Visit` borrows the tree, for analyses.
- `VisitMut` borrows it mutably, for rewriting in place.
- `Fold` takes it by value and rebuilds it.

psync uses the first two. `Rewriter` in `rewrite.rs` is a `VisitMut`. It overrides
`visit_block_mut`, first rewrites the inner blocks, and then schedules the statements of the
current one, which is why the book says psync works "innermost first".

Leaving out the call to the default method is how a visitor *stops* descending. `Rewriter` does
that at nested `fn` items, which need their own attribute. It still walks into closures, but it
only rewrites async ones. The visitor in `classify.rs` that looks for awaits and exits stops at
closures and `async` blocks. Their code does not run when the statement that contains them
runs, and a `?` inside a closure leaves the closure, not the function.

## Two views of the same statement

psync reads each statement twice, in two different ways:

- **As a tree** (`classify.rs`, `hoist.rs`, `loops.rs`). Questions about structure need the
  tree: is this a `let` or an expression? Is there an `.await` in the scrutinee of this `match`?
  Is the value of this block an `if`? Does a `?` sit inside a closure, where it would leave the
  closure rather than the function?
- **As tokens** (`access.rs`). Questions of the kind "which variables and fields does this touch"
  are answered by a scan over the statement's raw tokens. A scan works equally well inside macro
  calls like `println!("{x}")` or `vec![a, b]`. `syn` cannot see into those, because a macro's
  arguments are just a token stream until the macro is expanded. The scan follows `.field` and
  `[index]` after an identifier, which gives *access paths* like `self.groups`, and it treats
  assignments, `&mut`, and calls of mutating or order-sensitive methods as writes. The details
  are in [Reads and writes from tokens](../internals/access.md).

## What syntax cannot tell you

Look at `s4` in the output: it "reads" `Ok`. The visitor saw a path with one segment and took it
for a variable. In the tree, `Ok` (a constructor), `user` (a local variable) and `MAX` (a constant)
all look the same: `Expr::Path`. Telling them apart is **name resolution**, which happens inside
rustc, after macro expansion. psync's token scan skips keywords and records every other name as a
possible variable. Here that mistake is harmless: no statement binds `Ok`, so it never creates an
edge. But a constant and a variable of the same name would be confused, and in general tokens
cannot know.

The same goes for types. `user.avatar_url` might be a field of a struct, or a field reached by
auto-dereferencing through an `Arc`. Syntax cannot see the difference. In the next chapter,
psync gets to *use* a tree. The chapter after that climbs past the point where these questions
get answered.

## Exercises

1. Which statements must `let n = posts.len() + friends.len();` wait for, if it is added before
   the final `Ok(..)`? Check your answer by passing a function to the example, as in
   `cargo run -p psync-graph --example syntax_tree -- 'async fn f() { let n = posts.len() + friends.len(); }'`.
   Does the reported `reads` agree?
2. `syn` parses `a.b.c()` as a `MethodCall` whose receiver is a `Field`. Write the shape that
   `shape` would print, then check it the same way.
3. Why does a visitor need the `'ast` lifetime on `visit_expr_path(&mut self, p: &'ast ExprPath)`?
   What could an analysis store if the lifetime were not there?
4. A closure `|x| x.await` is not valid Rust, but `async move { x.await }` is. Why must psync
   *not* count the await inside `let f = async move { x.await };` as an await of the enclosing
   statement? (Hint: when does the body of an async block run?)

<details>
<summary>Answers</summary>

1. It reads `posts` and `friends`, so it waits for `s1` and `s3`. The example reports
   `reads ["friends", "posts"]` (`len` is a method name after a `.`, so it is not a path).
2. `MethodCall(.c, receiver: Field(Path(a), b), args: [])`. Field access and method calls are
   postfix operators, applied left to right: first `a.b`, then `.c()` on the result. So the field
   is the receiver.
3. With `'ast`, the visitor can keep references *into* the tree, for example a
   `Vec<&'ast Expr>` of every awaited expression, and use them after the walk. Without it, the
   borrow would end with each call.
4. An async block does nothing until it is awaited, so the statement only *creates* a future.
   The await runs later, whenever `f` is awaited. The await finder in `classify.rs` stops at
   `async` blocks for this reason. The statement therefore has no await of its own. Since it is not
   on the list of cheap pure expressions either, psync treats it as an ordinary statement, kept in
   order with the other ordinary statements.

</details>
