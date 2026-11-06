//! Dependency analysis and code generation behind `#[psync::graph]`.
//!
//! This is Phase I step 2 of the psync proposal: a source-level pass that finds the awaits of an
//! async function which do not depend on each other and runs them concurrently. It sees tokens
//! only, so it is deliberately conservative, and the Rust compiler still type- and borrow-checks
//! everything it generates (two branches that both need `&mut x` fail to compile; they never race).
//!
//! # Pipeline
//!
//! For every block of async code, innermost first (`rewrite.rs`):
//!
//! 1. **Classify** each statement (`classify.rs`) as a movable *node* or a *barrier* that stays in
//!    place. Nodes are bindings, awaits, and ordinary statements (*effects*, kept in order with
//!    each other). Barriers are block values, lock acquisitions, sleeps, and code psync cannot
//!    rewrite (exits hidden in macros, `yield`).
//! 2. **Expose** awaits hidden inside larger statements by hoisting them into bindings, in
//!    evaluation order (`hoist.rs`).
//! 3. **Graph** each run of nodes between barriers (`schedule.rs`): an edge where one node reads a
//!    binding another defines, where their accesses to a variable or field conflict
//!    (`access.rs`), and between effects.
//! 4. **Schedule**: single-predecessor runs fuse into chains; the chains of each level run
//!    concurrently, and their bindings are handed back in program order.
//! 5. **Loops** (`loops.rs`): iterations' awaited prefixes run up to `bound` ahead while the rest of
//!    each body runs in order; loop-carried state is split into a first pass (*fission*); work
//!    behind an early-exit guard runs ahead when the guard would not exit.
//!
//! # What stays the same
//!
//! Results, bindings and early exits. A `?`, `return`, `break` or `continue` inside a node stops
//! that node's chain and is performed after the join in program order (`exits.rs`), so the
//! function exits from the same place with the same value. What can differ is work that
//! sequential code would have skipped after an exit: by adding the attribute, the programmer
//! asserts that awaited operations tolerate running speculatively (reads, idempotent writes).
//! Ordinary statements never move past each other, and locks, permits, transactions, sleeps and
//! order-sensitive calls (`send`, `write`, `recv`, ...) keep their order.
//!
//! # Hints
//!
//! Where tokens cannot show what the programmer knows:
//!
//! - `#[psync::seq]` on a statement keeps it in place.
//! - `#[psync::fresh]` on a `let` makes each concurrent branch evaluate the initializer again
//!   (`let pool = &mut context.pool();` gives every query its own connection).
//! - `#[psync::independent]` on a loop asserts that iterations do not depend on each other
//!   (needed for `while` loops, whose condition psync cannot reason about).
//!
//! `#[psync::graph(off)]` compiles the function as written, for comparisons; building with the
//! environment variable `PSYNC_GRAPH=off` does the same for every annotated function.

mod access;
mod classify;
mod exits;
mod hoist;
mod loops;
mod rewrite;
mod schedule;

use classify::{Scope, is_psync_attr, pat_idents};
use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{
    Expr, ItemFn, Local, parse::Parser, punctuated::Punctuated, visit::Visit, visit_mut::VisitMut,
};

/// Options from `#[psync::graph(bound = N, explain, off, crate = path)]`.
#[derive(Clone, Debug)]
pub struct Options {
    /// Compile the function as written (hints are only removed), for comparisons.
    pub off: bool,
    /// Maximum loop iterations running ahead at once (default 16).
    pub bound: usize,
    /// Print the dependency graph to stderr at expansion time.
    pub explain: bool,
    /// Path to the psync crate in generated code (default `::psync`).
    pub krate: TokenStream,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            off: false,
            bound: 16,
            explain: false,
            krate: quote!(::psync),
        }
    }
}

impl Options {
    /// Parse the attribute's arguments.
    pub fn parse(attr: TokenStream) -> syn::Result<Self> {
        let mut options = Options::default();
        let metas = Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated.parse2(attr)?;
        for meta in metas {
            match &meta {
                syn::Meta::Path(p) if p.is_ident("explain") => options.explain = true,
                syn::Meta::Path(p) if p.is_ident("off") => options.off = true,
                syn::Meta::NameValue(nv) if nv.path.is_ident("bound") => {
                    let Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(n),
                        ..
                    }) = &nv.value
                    else {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            "bound must be an integer",
                        ));
                    };
                    options.bound = n.base10_parse()?;
                    if options.bound == 0 {
                        return Err(syn::Error::new_spanned(n, "bound must be at least 1"));
                    }
                }
                syn::Meta::NameValue(nv) if nv.path.is_ident("crate") => {
                    options.krate = nv.value.to_token_stream();
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        meta,
                        "expected `bound = N`, `explain`, `off` or `crate = path`",
                    ));
                }
            }
        }
        Ok(options)
    }
}

/// What the analysis found, for `explain` and external tooling (`psync-expand`).
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "cli", derive(serde::Serialize))]
pub struct Report {
    /// Every region with two or more awaits that was analyzed.
    pub regions: Vec<RegionReport>,
    /// Every `for` loop considered for prefetching.
    pub loops: Vec<LoopReport>,
    /// Await-containing statements kept in place, with the reason.
    pub barriers: Vec<String>,
}

/// The graph and schedule of one region.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "cli", derive(serde::Serialize))]
pub struct RegionReport {
    pub nodes: Vec<NodeReport>,
    /// `(from, to, reason)`: `to` waits for `from`.
    pub edges: Vec<(usize, usize, String)>,
    /// Fused chains: node indices in program order.
    pub chains: Vec<Vec<usize>>,
    /// Chain indices that run concurrently, level by level.
    pub levels: Vec<Vec<usize>>,
    /// Some level runs two or more chains concurrently.
    pub changed: bool,
}

/// One node of a region.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "cli", derive(serde::Serialize))]
pub struct NodeReport {
    pub label: String,
    pub awaits: bool,
    pub effect: bool,
    pub exits: bool,
    pub line: usize,
}

impl From<&classify::Node> for NodeReport {
    fn from(n: &classify::Node) -> Self {
        Self {
            label: n.label.clone(),
            awaits: n.awaits,
            effect: n.effect,
            exits: n.exits,
            line: n.line,
        }
    }
}

/// One `for` loop and whether its iterations were prefetched.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "cli", derive(serde::Serialize))]
pub struct LoopReport {
    pub label: String,
    /// Labels of the statements run ahead for each iteration.
    pub prefetched: Vec<String>,
    pub bound: usize,
    pub changed: bool,
    /// Why the loop was left alone.
    pub reason: String,
    /// First and last source line of the loop (0 when spans carry no location).
    pub line: usize,
    pub end_line: usize,
}

/// Expand `#[psync::graph(attr)] item`.
pub fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let mut options = Options::parse(attr)?;
    let mut function: ItemFn = syn::parse2(item)?;
    // `PSYNC_GRAPH=off` at build time compiles every annotated function as written, so one
    // source tree can be built both ways for comparison.
    if std::env::var("PSYNC_GRAPH").is_ok_and(|v| v == "off") {
        options.off = true;
    }
    if options.off {
        function.attrs.retain(|a| !is_psync_attr(a, "graph"));
        schedule::StripHints.visit_block_mut(&mut function.block);
        return Ok(function.into_token_stream());
    }
    let report = transform_fn(&mut function, &options);
    if options.explain {
        eprintln!(
            "psync::graph `{}`:\n{}",
            function.sig.ident,
            explain(&report)
        );
    }
    Ok(function.into_token_stream())
}

/// Rewrite every async block in `function` (its body if it is `async`, and any async blocks or
/// closures inside), returning what was found.
pub fn transform_fn(function: &mut ItemFn, options: &Options) -> Report {
    let mut scope = Scope {
        mutable: Default::default(),
        handles: Default::default(),
        fresh: Default::default(),
    };
    for input in &function.sig.inputs {
        match input {
            // `&mut self` is a unique handle; `mut self` is an owned mutable binding.
            syn::FnArg::Receiver(r) if r.mutability.is_some() => {
                let set = if r.reference.is_some() {
                    &mut scope.handles
                } else {
                    &mut scope.mutable
                };
                set.insert("self".into());
            }
            syn::FnArg::Typed(t) => {
                let mut ids = Vec::new();
                pat_idents(&t.pat, &mut ids);
                let mut_ref = matches!(&*t.ty, syn::Type::Reference(r) if r.mutability.is_some());
                for (id, is_mut) in ids {
                    if mut_ref {
                        scope.handles.insert(id.to_string());
                    } else if is_mut {
                        scope.mutable.insert(id.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    Declarations(&mut scope).visit_block(&function.block);
    let mut rewriter = rewrite::Rewriter {
        options,
        scope,
        report: Report::default(),
        in_async: function.sig.asyncness.is_some(),
        fresh: 0,
    };
    rewriter.visit_block_mut(&mut function.block);
    function.attrs.retain(|a| !is_psync_attr(a, "graph"));
    schedule::StripHints.visit_block_mut(&mut function.block);
    rewriter.report
}

/// Collects `let mut` bindings, bindings to `&mut` expressions, and `#[psync::fresh]` bindings
/// anywhere in the function.
struct Declarations<'a>(&'a mut Scope);
impl<'ast> Visit<'ast> for Declarations<'_> {
    fn visit_local(&mut self, local: &'ast Local) {
        let mut ids = Vec::new();
        pat_idents(&local.pat, &mut ids);
        if local.attrs.iter().any(|a| is_psync_attr(a, "fresh"))
            && let ([(id, _)], Some(init)) = (&ids[..], &local.init)
        {
            self.0
                .fresh
                .insert(id.to_string(), (id.clone(), init.expr.to_token_stream()));
            return;
        }
        let handle = local
            .init
            .as_ref()
            .is_some_and(|i| matches!(&*i.expr, Expr::Reference(r) if r.mutability.is_some()));
        for (id, is_mut) in ids {
            if handle {
                self.0.handles.insert(id.to_string());
            } else if is_mut {
                self.0.mutable.insert(id.to_string());
            }
        }
        syn::visit::visit_local(self, local);
    }
    fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
        if p.mutability.is_some() {
            self.0.mutable.insert(p.ident.to_string());
        }
        syn::visit::visit_pat_ident(self, p);
    }
}

/// Human-readable summary of a report (what `explain` prints).
pub fn explain(report: &Report) -> String {
    let mut out = String::new();
    for (r, region) in report.regions.iter().enumerate() {
        out += &format!(
            "  region {r}{}:\n",
            if region.changed { "" } else { " (unchanged)" }
        );
        for (i, n) in region.nodes.iter().enumerate() {
            out += &format!(
                "    n{i}: {}{}\n",
                n.label,
                if n.awaits { " [await]" } else { "" }
            );
        }
        for (a, b, why) in &region.edges {
            out += &format!("    n{a} -> n{b} ({why})\n");
        }
        for (l, level) in region.levels.iter().enumerate() {
            let chains: Vec<String> = level
                .iter()
                .map(|c| {
                    region.chains[*c]
                        .iter()
                        .map(|n| format!("n{n}"))
                        .collect::<Vec<_>>()
                        .join("->")
                })
                .collect();
            out += &format!("    level {l}: {}\n", chains.join(" || "));
        }
    }
    for l in &report.loops {
        let what = if l.changed {
            format!(
                "prefetch [{}], {} at a time",
                l.prefetched.join(", "),
                l.bound
            )
        } else {
            l.reason.clone()
        };
        out += &format!("  loop `{}`: {what}\n", l.label);
    }
    for b in &report.barriers {
        out += &format!("  barrier: {b}\n");
    }
    out
}
