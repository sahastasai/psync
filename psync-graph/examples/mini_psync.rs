//! Book, Part 0, "Procedural macros": psync in about a hundred lines.
//!
//! `cargo run -p psync-graph --example mini_psync`
//!
//! `mini_graph` has exactly the signature of an attribute macro's body: tokens in, tokens out. It
//! handles one shape only: a block of `let name = <future>.await;` statements and a final value.
//! It finds which statements read names that earlier ones bind, puts each statement on a *level*
//! one past the deepest statement it reads from, and joins every level of two or more futures.
//!
//! What the real `#[psync::graph]` adds is most of the book: `?` and other exits (this toy refuses
//! them, see the chapter's exercise), writes and effects, chains instead of levels, hidden awaits,
//! loops, barriers, hints, and the compiler facts.
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::BTreeSet;
use syn::visit::Visit;
use syn::{Expr, ItemFn, Pat, Stmt};

/// One `let name = future.await;`.
struct Node {
    name: syn::Ident,
    future: Expr,
    reads: BTreeSet<String>,
}

/// Every single-segment path the expression mentions: the variables it may read.
fn reads_of(e: &Expr) -> BTreeSet<String> {
    struct Reads(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Reads {
        fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
            if let Some(i) = p.path.get_ident() {
                self.0.insert(i.to_string());
            }
        }
    }
    let mut r = Reads(BTreeSet::new());
    r.visit_expr(e);
    r.0
}

/// The attribute macro's body: `#[mini_graph] async fn ..` → a function whose independent awaits
/// are joined.
fn mini_graph(item: TokenStream) -> syn::Result<TokenStream> {
    let mut function: ItemFn = syn::parse2(item)?;
    let mut stmts = function.block.stmts.clone();
    let tail = stmts.pop().expect("a final value");

    // 1. Analysis: read each statement into a node.
    let mut nodes = Vec::new();
    for stmt in &stmts {
        let Stmt::Local(local) = stmt else {
            return Err(syn::Error::new_spanned(stmt, "mini_graph: only `let`s"));
        };
        let (Pat::Ident(p), Some(init)) = (&local.pat, &local.init) else {
            return Err(syn::Error::new_spanned(
                stmt,
                "mini_graph: `let name = ..;` only",
            ));
        };
        let Expr::Await(awaited) = &*init.expr else {
            return Err(syn::Error::new_spanned(
                &init.expr,
                "mini_graph: the value must be `<future>.await` (no `?`)",
            ));
        };
        nodes.push(Node {
            name: p.ident.clone(),
            future: (*awaited.base).clone(),
            reads: reads_of(&awaited.base),
        });
    }

    // 2. The graph: an edge j -> i when node i reads the name node j binds. Each node's level is
    //    one past the deepest level it depends on. (Names must be distinct: no shadowing here.)
    let mut level = vec![0usize; nodes.len()];
    for i in 0..nodes.len() {
        for j in 0..i {
            if nodes[i].reads.contains(&nodes[j].name.to_string()) {
                println!("  edge s{j} -> s{i} (reads `{}`)", nodes[j].name);
                level[i] = level[i].max(level[j] + 1);
            }
        }
    }

    // 3. Code generation: one statement per level, in level order. Within a level the nodes keep
    //    program order, so the bindings come back in the order they were written.
    let depth = level.iter().max().map_or(0, |d| d + 1);
    let mut out = Vec::new();
    for l in 0..depth {
        let here: Vec<&Node> = (0..nodes.len())
            .filter(|&i| level[i] == l)
            .map(|i| &nodes[i])
            .collect();
        let names: Vec<_> = here.iter().map(|n| &n.name).collect();
        let futures: Vec<_> = here.iter().map(|n| &n.future).collect();
        println!("  level {l}: {}", quote!(#(#names)||*));
        out.push(match here.as_slice() {
            [one] => {
                let (name, future) = (&one.name, &one.future);
                quote! { let #name = #future.await; }
            }
            _ => quote! { let (#(#names),*) = ::futures::join!(#(#futures),*); },
        });
    }
    // A marker item, as the real macro leaves `__PSYNC_GRAPH` for the compiler driver to find.
    let marker = format_ident!("__MINI_GRAPH");
    function.block = syn::parse2(quote! {{
        const #marker: () = ();
        #(#out)*
        #tail
    }})?;
    Ok(quote!(#function))
}

fn main() {
    let input = quote! {
        async fn profile_page(api: &Api, user_id: u64) -> Page {
            let user = api.user(user_id).await;
            let posts = api.posts(user_id).await;
            let avatar = api.avatar(&user.avatar_url).await;
            let friends = api.friends(user_id).await;
            Page { user, posts, avatar, friends }
        }
    };
    println!("analysis:");
    let output = mini_graph(input).expect("supported shape");
    let file: syn::File = syn::parse2(output).expect("valid Rust");
    println!("\ngenerated:\n{}", prettyplease::unparse(&file));

    // The toy refuses what it does not understand, with an error at the offending tokens: in a
    // real macro, `to_compile_error()` turns this into a compiler error pointing there.
    let refused = mini_graph(quote! {
        async fn f(api: &Api) -> u32 { let a = api.one().await?; a }
    });
    println!("refused: {}", refused.err().expect("an error"));
}
