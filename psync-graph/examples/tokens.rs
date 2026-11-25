//! Book, Part 0, "Tokens": what a procedural macro receives.
//!
//! `cargo run -p psync-graph --example tokens [-- "SOURCE"]`
//!
//! A proc macro is handed a `TokenStream`: a sequence of token trees. This program builds one from
//! a string (as `proc_macro2` lets an ordinary program do) and prints every tree, indenting the
//! contents of each group.
use proc_macro2::{Delimiter, TokenStream, TokenTree};

fn show(stream: TokenStream, depth: usize) {
    for tree in stream {
        let pad = "  ".repeat(depth);
        match tree {
            TokenTree::Ident(i) => println!("{pad}Ident    {i}"),
            TokenTree::Punct(p) => {
                // `Joint` means the next character is glued on: `?` `.` vs `->`, `::`, `+=`.
                println!("{pad}Punct    {}  ({:?})", p.as_char(), p.spacing())
            }
            TokenTree::Literal(l) => println!("{pad}Literal  {l}"),
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::None => ("«", "»"),
                };
                println!("{pad}Group    {open} … {close}");
                show(g.stream(), depth + 1);
            }
        }
    }
}

fn main() {
    let source = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "let user = api.user(id).await?;".to_string());
    let stream: TokenStream = source.as_str().parse().expect("valid tokens");
    println!("{source}\n");
    show(stream, 0);
}
