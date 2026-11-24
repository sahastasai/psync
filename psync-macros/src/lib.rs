//! `#[psync::graph]`: run the independent awaits of an async function concurrently.
//! The analysis and code generation live in `psync-graph`; see its documentation.
use proc_macro::TokenStream;

/// Build the function's await dependency graph and run independent work concurrently.
///
/// Options:
/// - `bound = N` caps concurrent loop iterations (default 16);
/// - `explain` prints the graph at compile time;
/// - `off` compiles the function as written (for comparisons; `PSYNC_GRAPH=off` does it for all);
/// - `strict` treats awaited calls psync cannot classify as writes to an unknown resource;
/// - `speculate` lets writes run before earlier exits everywhere in the function;
/// - `effects(read(a, ..), write(b, ..), ordered(c, ..))` names more verbs for the effect analysis;
/// - `crate = path` names the psync crate in generated code.
///
/// Hints inside the function: `#[psync::seq]`, `#[psync::fresh]`, `#[psync::independent]`,
/// `#[psync::speculate]`.
///
/// ```ignore
/// #[psync::graph(bound = 8)]
/// async fn sizes(store: &Store, keys: &[String]) -> Result<u64, Error> {
///     let mut total = 0;
///     for key in keys {
///         total += store.head(key).await?.size; // up to 8 HEADs in flight
///     }
///     Ok(total)
/// }
/// ```
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
/// On a statement: its effects on outside resources commute with those of the statements around
/// it (two writes to different objects through one client), so they need not keep their order.
/// Outside `#[psync::graph]` it does nothing.
#[proc_macro_attribute]
pub fn independent(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Hint on a statement or `for` loop: its writes (to files, services, the network) may run ahead
/// of an earlier `?`, `return` or `break`, or for later loop items before an earlier item ends the
/// loop. The programmer asserts that performing them when sequential code would not have is
/// harmless (idempotent uploads, cache fills). Outside `#[psync::graph]` it does nothing.
#[proc_macro_attribute]
pub fn speculate(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
