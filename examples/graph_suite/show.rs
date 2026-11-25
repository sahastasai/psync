//! Declaring and presenting examples.
//!
//! [`example!`] compiles one function twice from the same source: `sequential` with
//! `#[psync::graph(off)]` (exactly as written) and `graph` with `#[psync::graph]`. It also keeps
//! the source text, so the suite can print the original and the code psync generates from it
//! (by calling the same generator the attribute uses).

use std::{future::Future, pin::Pin, time::Duration};

/// A boxed run of one version of an example, with its result rendered for comparison.
pub type Run = Box<dyn Fn() -> Pin<Box<dyn Future<Output = String>>>>;

pub struct Example {
    /// Where the shape comes from (repository, function, audit id).
    pub origin: &'static str,
    /// The concurrency pattern.
    pub pattern: &'static str,
    /// What psync overlaps.
    pub summary: &'static str,
    /// The function's source (the attribute's options, then the item).
    pub options: &'static str,
    pub source: &'static str,
    pub sequential: Run,
    pub graph: Run,
}

/// Declare an example. `call` builds the future from `f`, which names either version.
macro_rules! example {
    (
        origin: $origin:literal,
        pattern: $pattern:literal,
        summary: $summary:literal,
        options: ($($options:tt)*),
        call: |$f:ident| $call:expr,
        async fn $name:ident $($rest:tt)*
    ) => {
        pub mod $name {
            #[allow(unused_imports)]
            use super::*;
            pub mod sequential {
                #[allow(unused_imports)]
                use super::*;
                #[psync::graph(off)]
                pub async fn $name $($rest)*
            }
            pub mod graph {
                #[allow(unused_imports)]
                use super::*;
                #[psync::graph($($options)*)]
                pub async fn $name $($rest)*
            }
            pub fn example() -> $crate::show::Example {
                $crate::show::Example {
                    origin: $origin,
                    pattern: $pattern,
                    summary: $summary,
                    options: stringify!($($options)*),
                    source: stringify!(async fn $name $($rest)*),
                    sequential: Box::new(|| {
                        let $f = sequential::$name;
                        Box::pin(async move { format!("{:?}", $call.await) })
                    }),
                    graph: Box::new(|| {
                        let $f = graph::$name;
                        Box::pin(async move { format!("{:?}", $call.await) })
                    }),
                }
            }
        }
    };
}
pub(crate) use example;

/// Pretty-print a function's source.
pub fn pretty(item: &syn::ItemFn) -> String {
    prettyplease::unparse(&syn::File {
        shebang: None,
        attrs: Vec::new(),
        items: vec![syn::Item::Fn(item.clone())],
    })
}

/// The original function and psync's rewrite of it, pretty-printed, plus the graph summary.
pub fn rewrite(example: &Example) -> (String, String, String) {
    let original: syn::ItemFn = syn::parse_str(example.source).expect("example source parses");
    let mut options =
        psync_graph::Options::parse(example.options.parse().expect("options")).expect("options");
    options.explain = false;
    let mut rewritten = original.clone();
    let report = psync_graph::transform_fn(&mut rewritten, &options);
    (
        pretty(&original),
        pretty(&rewritten),
        psync_graph::explain(&report),
    )
}

pub fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1e3)
}

/// Indent every line of `text`.
pub fn indent(text: &str) -> String {
    text.lines().map(|l| format!("    {l}\n")).collect()
}
