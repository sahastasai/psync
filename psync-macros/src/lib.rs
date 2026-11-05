//! `#[psync::graph]`: run the independent awaits of an async function concurrently.
//! The analysis and code generation live in `psync-graph`; see its documentation.
use proc_macro::TokenStream;

/// Build the function's await dependency graph and run independent work concurrently.
///
/// Options: `bound = N` caps concurrent loop iterations (default 16); `explain` prints the
/// graph at compile time; `off` compiles the function as written (for comparisons).
/// Hints inside the function: `#[psync::seq]`, `#[psync::fresh]`, `#[psync::independent]`.
#[proc_macro_attribute]
pub fn graph(attr: TokenStream, item: TokenStream) -> TokenStream {
    psync_graph::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Hint: keep this statement in its original position inside a `#[psync::graph]` function.
/// Outside one it does nothing.
#[proc_macro_attribute]
pub fn seq(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Hint on a `let`: each concurrent branch that uses this binding evaluates its initializer
/// again instead of sharing the value, e.g. `#[psync::fresh] let pool = &mut context.pool();`
/// gives every branch its own pooled connection. Outside `#[psync::graph]` it does nothing.
#[proc_macro_attribute]
pub fn fresh(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Hint on a `for` loop: the programmer asserts that an iteration's awaited work does not depend
/// on what earlier iterations wrote, so psync may run it ahead even where it cannot prove that.
/// Outside `#[psync::graph]` it does nothing.
#[proc_macro_attribute]
pub fn independent(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
