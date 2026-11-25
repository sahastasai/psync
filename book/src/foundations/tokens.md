# Tokens

A Rust source file is, to begin with, a string of characters. The first thing any compiler does
is cut that string into **tokens**, the smallest pieces that mean something: a word, a number, a
symbol. This step is called **lexing**, and the part of the compiler that does it is the
**lexer**.

```text
"let user = api.user(id).await?;"
   │ lexing
   ▼
let │ user │ = │ api │ . │ user │ ( │ id │ ) │ . │ await │ ? │ ;
```

Whitespace and comments are gone. `api.user` is three tokens, not one. Nothing yet says that
`let` starts a statement or that `user(id)` is a call. The lexer only knows which characters
belong together.

## Token trees

Rust's macros do not see a flat list. They see **token trees**. Every pair of matching brackets,
`( )`, `[ ]` or `{ }`, becomes one **group**, and the tokens between them are the group's
children. There are four kinds of tree:

| Kind | Examples | Notes |
|---|---|---|
| `Ident` | `let`, `user`, `await`, `self` | names and keywords alike: the lexer does not tell them apart |
| `Punct` | `=`, `.`, `?`, `;` | one character each; `->` is `-` then `>`, marked *joint* |
| `Literal` | `42`, `"cache.json"`, `'a'`, `1.5e3` | the text is kept as written |
| `Group` | `( id )`, `{ .. }`, `[ .. ]` | a delimiter and a nested token stream |

Brackets are grouped already, so a macro can never be handed `( id` without its `)`. Grouping is
the only structure tokens have.

Run the example:

```sh
cargo run -p psync-graph --example tokens
```

```text
let user = api.user(id).await?;

Ident    let
Ident    user
Punct    =  (Alone)
Ident    api
Punct    .  (Alone)
Ident    user
Group    ( … )
  Ident    id
Punct    .  (Alone)
Ident    await
Punct    ?  (Joint)
Punct    ;  (Alone)
```

The program (`psync-graph/examples/tokens.rs`) parses a string into a
`proc_macro2::TokenStream` and walks it with a `match` on `TokenTree`. Each `Punct` records its
**spacing**. `Joint` means another punctuation character follows with no space, which is how
`+=`, `::` and `->` are represented. Here `?` is joint because `;` follows it directly.

## Spans: where a token came from

Every token carries a **span**, a reference to the place in the source it came from. Spans are
how the compiler can say `error at src/lib.rs:24:20` about code a macro produced. Inside a macro
you can attach your own errors to the tokens that caused them, and they then point at your user's
code, not at the macro.

psync relies on spans twice:

- When `#[psync::graph]` refuses something, its error has the span of the offending statement.
- Compiler facts (see [Scheduling with compiler facts](../safety/facts.md)) are keyed by the
  file, line and column of a callee's name. The driver computes the key from MIR spans, and the
  macro computes the same key from token spans. That match is the only link between the two
  halves of psync.

## `proc_macro` and `proc_macro2`

rustc gives procedural macros the type `proc_macro::TokenStream`. That type only exists while
the compiler is running a macro, so you cannot use it in a test, a binary or an example.
`proc_macro2::TokenStream` is the same interface, usable anywhere. Real macros convert at the
boundary. Here is psync's entire macro crate, apart from the hint attributes (which return their input
unchanged):

```rust,ignore
#[proc_macro_attribute]
pub fn graph(attr: TokenStream, item: TokenStream) -> TokenStream {
    psync_graph::expand(attr.into(), item.into())   // proc_macro → proc_macro2
        .unwrap_or_else(|e| e.to_compile_error())
        .into()                                       // proc_macro2 → proc_macro
}
```

This is why `psync-graph` is an ordinary library with ordinary unit tests, and why
`psync-expand` can run psync's analysis on a file outside the compiler.

## What tokens cannot tell you

Look again at the token stream above and ask three questions:

1. *What type is `api`?* Unknown. It is an identifier. The type is declared somewhere else, maybe
   in another crate.
2. *Which function does `.user(id)` call?* Unknown. That depends on the type of `api`, on which
   traits are in scope, and on auto-referencing rules.
3. *Does that call write to a file?* Unknown. Even if we knew which function it was, its body is
   elsewhere.

A procedural macro gets the tokens of the item it is attached to, and nothing else. That is the
fundamental limit of `#[psync::graph]`. It is why the macro reads names (`user` sounds like a read,
`delete` sounds like a write), why it has hints, and why `cargo psync` exists. The next chapters
climb the pipeline until each of these questions has an answer.

## Exercises

1. Run `cargo run -p psync-graph --example tokens -- 'x += a::b(&c[0]);'`. Before you do,
   predict the trees. How many groups are there? Which `Punct`s are `Joint`?
2. Is `'a` (a lifetime) one token or two? Try it with `-- "fn f<'a>()"`.
3. Why can a macro never receive the tokens `foo(bar`? What does this guarantee to the
   writer of a macro?
4. The keyword `await` is lexed as an `Ident`, like `user`. Give one reason this is convenient for
   the lexer, and one place in psync where the code has to tell them apart.

<details>
<summary>Answers</summary>

1. Two groups: `( … )` around `&c[0]`, and inside it `[ … ]` around `0`. `+=` is `+` (Joint) then
   `=` (Alone). `::` is `:` (Joint) then `:` (Alone). The other `Punct`s (`&`, `;`) are Alone. The
   lexer marks a `Punct` Joint whenever another punctuation character follows directly, so a `?`
   right before `;` is Joint too, as the output above shows. Spacing records adjacency, not
   meaning.
2. Two trees: a `Punct` `'` (Joint) followed by an `Ident` `a`. The Joint spacing is what says they
   belong together, and `syn` reassembles them into one `Lifetime`.
3. The lexer matches brackets before any macro runs, and an unbalanced bracket is an error. A
   macro can therefore split its input at groups without counting brackets, and it can always
   copy a group through unchanged.
4. Keywords depend on the edition (`async` and `await` became keywords in 2018), and the lexer
   need not know the edition. psync tells them apart when it looks for awaits. `syn` parses
   `x.await` into an `Expr::Await` node, and `classify.rs` matches on that node, not on the token.

</details>
