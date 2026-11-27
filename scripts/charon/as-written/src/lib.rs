//! `#[psync::graph]` that rewrites nothing: it removes psync's statement attributes
//! (`#[psync::par]`, ...) and leaves the function as written. Charon compiles the fixture with it, so
//! the statements it extracts are the program in order, which is what a certificate is about.
use proc_macro::{Delimiter, Group, TokenStream, TokenTree};

#[proc_macro_attribute]
pub fn graph(_options: TokenStream, item: TokenStream) -> TokenStream {
    strip(item)
}

/// The tokens without `#[psync::...]` attributes, with their spans (so lines stay those of the
/// source file).
fn strip(tokens: TokenStream) -> TokenStream {
    let mut out = Vec::new();
    let mut it = tokens.into_iter().peekable();
    while let Some(tt) = it.next() {
        if let TokenTree::Punct(p) = &tt
            && p.as_char() == '#'
            && let Some(TokenTree::Group(g)) = it.peek()
            && g.delimiter() == Delimiter::Bracket
            && matches!(g.stream().into_iter().next(), Some(TokenTree::Ident(i)) if i.to_string() == "psync")
        {
            it.next();
            continue;
        }
        out.push(match tt {
            TokenTree::Group(g) => {
                let mut n = Group::new(g.delimiter(), strip(g.stream()));
                n.set_span(g.span());
                TokenTree::Group(n)
            }
            t => t,
        });
    }
    out.into_iter().collect()
}

/// `#[psync::observed]`: the item as written.
#[proc_macro_attribute]
pub fn observed(_options: TokenStream, item: TokenStream) -> TokenStream {
    item
}
