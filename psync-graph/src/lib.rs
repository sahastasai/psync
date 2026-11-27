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
//!    (`access.rs`), where their effects on files, services, the network, processes or the
//!    environment conflict (`effects.rs`), before a write that an earlier exit could skip, and
//!    between ordinary statements.
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
//! function exits from the same place with the same value. Recognized writes (`fs::write`,
//! `api.delete`, `s3.put_object`, processes) never start before an earlier exit, and operations on
//! one file or service keep their order (`effects.rs`). What can still differ is unclassified
//! awaited work that sequential code would have skipped after an exit: by adding the attribute,
//! the programmer asserts that such calls tolerate running speculatively; `explain` lists them and
//! `strict` treats them as writes. Ordinary statements never move past each other, and locks,
//! permits, transactions, sleeps and order-sensitive calls (`send`, `write`, `recv`, ...) keep
//! their order.
//!
//! # Hints
//!
//! Where tokens cannot show what the programmer knows:
//!
//! - `#[psync::seq]` on a statement keeps it in place.
//! - `#[psync::fresh]` on a `let` makes each concurrent branch evaluate the initializer again
//!   (`let pool = &mut context.pool();` gives every query its own connection).
//! - `#[psync::independent]` on a loop asserts that iterations do not depend on each other
//!   (needed for `while` loops, whose condition psync cannot reason about); on a statement, that
//!   its effects commute with those around it.
//! - `#[psync::speculate]` on a statement or loop asserts that its writes may run although
//!   sequential code would have exited first (idempotent uploads, cache fills).
//!
//! Function options: `bound = N`, `explain`, `off`, `strict` (unclassified awaited calls are
//! writes), `speculate`, `effects(read(..), write(..), ordered(..))` (extra verbs), `crate = path`.
//!
//! `#[psync::graph(off)]` compiles the function as written, for comparisons; building with the
//! environment variable `PSYNC_GRAPH=off` does the same for every annotated function.

mod access;
mod classify;
mod effects;
pub mod facts;
mod gates;

extern crate proc_macro;
mod exits;
mod hoist;
mod loops;
mod rewrite;
mod schedule;

use classify::{Scope, is_psync_attr, pat_idents};
pub use effects::Config as EffectConfig;
pub use effects::{Verb, fs_reads, verb_of};
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
    /// `strict`, `speculate` and `effects(read(..), write(..), ordered(..))`.
    pub effects: EffectConfig,
    /// `yield_every = N`: every loop of async code yields to the executor every `N` iterations.
    pub yield_every: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            off: false,
            bound: 16,
            explain: false,
            krate: quote!(::psync),
            effects: EffectConfig::default(),
            yield_every: None,
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
                syn::Meta::Path(p) if p.is_ident("strict") => options.effects.strict = true,
                syn::Meta::Path(p) if p.is_ident("speculate") => options.effects.speculate = true,
                syn::Meta::List(list) if list.path.is_ident("effects") => {
                    list.parse_nested_meta(|kind| {
                        let target = if kind.path.is_ident("read") {
                            &mut options.effects.reads
                        } else if kind.path.is_ident("write") {
                            &mut options.effects.writes
                        } else if kind.path.is_ident("ordered") {
                            &mut options.effects.ordered
                        } else {
                            return Err(
                                kind.error("expected `read(..)`, `write(..)` or `ordered(..)`")
                            );
                        };
                        kind.parse_nested_meta(|verb| {
                            let name = verb
                                .path
                                .get_ident()
                                .ok_or_else(|| verb.error("expected a method or function name"))?;
                            target.push(name.to_string());
                            Ok(())
                        })
                    })?;
                }
                // `observe(log, PEAK)`: hidden state measured on purpose.
                syn::Meta::List(list) if list.path.is_ident("observe") => {
                    list.parse_nested_meta(|name| {
                        let id = name
                            .path
                            .get_ident()
                            .ok_or_else(|| name.error("expected a binding or static name"))?;
                        options.effects.observe.push(id.to_string());
                        Ok(())
                    })?;
                }
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
                syn::Meta::NameValue(nv) if nv.path.is_ident("yield_every") => {
                    let Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(n),
                        ..
                    }) = &nv.value
                    else {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            "yield_every must be an integer",
                        ));
                    };
                    let n: u32 = n.base10_parse()?;
                    if n == 0 {
                        return Err(syn::Error::new_spanned(
                            nv,
                            "yield_every must be at least 1",
                        ));
                    }
                    options.yield_every = Some(n);
                }
                syn::Meta::NameValue(nv) if nv.path.is_ident("crate") => {
                    options.krate = nv.value.to_token_stream();
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        meta,
                        "expected `bound = N`, `explain`, `off`, `strict`, `speculate`, `effects(..)`, \
                         `yield_every = N` or `crate = path`",
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
    /// `(line, call)`: awaited calls whose effect psync could not tell from their names. They are
    /// assumed to read, unless the function is `strict`.
    pub unclassified: Vec<(usize, String)>,
    /// The certificate of this region's schedule: each node's places (see `verify/Cert.lean`).
    /// With `chains` and `levels`, `psync-cert` turns it into a Lean check.
    pub cert: Vec<CertNode>,
    /// Pairs of chains that run concurrently because their computed file paths differ when
    /// written, and that are ordered at run time if the paths turn out to overlap.
    pub runtime_checks: usize,
}

/// One node as a certificate describes it: the places it may read and write, each a path of
/// segments. Two nodes are independent when neither writes a place the other reads or writes, and
/// places overlap when one is a prefix of the other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(serde::Serialize))]
pub struct CertNode {
    pub reads: Vec<Vec<String>>,
    pub writes: Vec<Vec<String>>,
    /// Those of its places that hold state measured on purpose (`#[psync::observed]`,
    /// `observe(..)`): listed, and hidden from the check that the schedule is equivalent.
    pub observed: Vec<Vec<String>>,
}

/// One node of a region.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "cli", derive(serde::Serialize))]
pub struct NodeReport {
    pub label: String,
    pub awaits: bool,
    /// Synchronous code: it blocks, and may run on another thread.
    pub blocks: bool,
    pub effect: bool,
    pub exits: bool,
    pub line: usize,
    /// Its last source line.
    pub end_line: usize,
    /// Its effects on files, the network, services, processes and the environment.
    pub effects: Vec<String>,
    /// Its writes may run ahead of earlier exits (`#[psync::speculate]`).
    pub speculate: bool,
}

impl From<&classify::Node> for NodeReport {
    fn from(n: &classify::Node) -> Self {
        Self {
            label: n.label.clone(),
            awaits: n.awaits,
            blocks: n.blocks,
            effect: n.effect,
            exits: n.exits,
            line: n.line,
            end_line: n.end_line,
            effects: n.effects.iter().map(effects::Effect::describe).collect(),
            speculate: n.speculate,
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
    /// For a prefetched loop whose items name their own files: `checked at run time`, or why
    /// that could not be checked and distinct items are assumed.
    pub check: String,
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
        effects: options.effects.clone(),
        sync: function.sig.asyncness.is_none(),
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
        // In a synchronous function the body itself is rewritten (Phase II).
        in_async: true,
        fresh: 0,
        refusal: None,
        loop_check: String::new(),
    };
    rewriter.visit_block_mut(&mut function.block);
    if let Some(every) = options.yield_every {
        YieldInjector {
            krate: &options.krate,
            every,
            in_async: function.sig.asyncness.is_some(),
        }
        .visit_block_mut(&mut function.block);
    }
    // `observe(x)`: tells the driver which bindings' state is measured on purpose.
    for name in options.effects.observe.iter().rev() {
        let marker = quote::format_ident!("__PSYNC_OBSERVED_{}", name);
        function.block.stmts.insert(
            0,
            syn::parse_quote! {
                #[allow(dead_code, non_upper_case_globals)]
                const #marker: () = ();
            },
        );
    }
    // Marks the function for `cargo psync`'s driver (which records facts about its calls), and
    // makes cargo rebuild it when the facts change (`option_env!` is tracked).
    function.block.stmts.insert(
        0,
        syn::parse_quote! {
            #[allow(dead_code)]
            const __PSYNC_GRAPH: ::core::option::Option<&str> =
                ::core::option_env!("PSYNC_FACTS_STAMP");
        },
    );
    function.attrs.retain(|a| !is_psync_attr(a, "graph"));
    schedule::StripHints.visit_block_mut(&mut function.block);
    rewriter.report
}

/// `yield_every = N`: at the start of every loop body in async code, yield to the executor on
/// every `N`-th iteration. A cooperative executor cannot interrupt a task that does not await;
/// these yields are preemption points the compiler inserts, so a long loop no longer holds
/// back higher-priority tasks. They change timing only, never results.
struct YieldInjector<'a> {
    krate: &'a TokenStream,
    every: u32,
    in_async: bool,
}

impl YieldInjector<'_> {
    fn inject(&self, body: &mut syn::Block) {
        let (krate, every) = (self.krate, self.every);
        body.stmts.insert(
            0,
            syn::parse_quote! {
                if #krate::__private::tick(
                    {
                        static __PSYNC_TICKS: ::core::sync::atomic::AtomicU32 =
                            ::core::sync::atomic::AtomicU32::new(0);
                        &__PSYNC_TICKS
                    },
                    #every,
                ) {
                    #krate::__private::yield_now().await;
                }
            },
        );
    }
}

impl VisitMut for YieldInjector<'_> {
    fn visit_expr_async_mut(&mut self, e: &mut syn::ExprAsync) {
        let outer = std::mem::replace(&mut self.in_async, true);
        syn::visit_mut::visit_expr_async_mut(self, e);
        self.in_async = outer;
    }
    fn visit_expr_closure_mut(&mut self, e: &mut syn::ExprClosure) {
        let outer = std::mem::replace(&mut self.in_async, e.asyncness.is_some());
        syn::visit_mut::visit_expr_closure_mut(self, e);
        self.in_async = outer;
    }
    fn visit_item_mut(&mut self, _: &mut syn::Item) {}
    fn visit_expr_for_loop_mut(&mut self, e: &mut syn::ExprForLoop) {
        syn::visit_mut::visit_expr_for_loop_mut(self, e);
        if self.in_async {
            self.inject(&mut e.body);
        }
    }
    fn visit_expr_while_mut(&mut self, e: &mut syn::ExprWhile) {
        syn::visit_mut::visit_expr_while_mut(self, e);
        if self.in_async {
            self.inject(&mut e.body);
        }
    }
    fn visit_expr_loop_mut(&mut self, e: &mut syn::ExprLoop) {
        syn::visit_mut::visit_expr_loop_mut(self, e);
        if self.in_async {
            self.inject(&mut e.body);
        }
    }
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
                "    n{i}: {}{}{}\n",
                n.label,
                if n.blocks {
                    " [blocks]"
                } else if n.awaits {
                    " [await]"
                } else {
                    ""
                },
                if n.effects.is_empty() {
                    String::new()
                } else {
                    format!(" {{{}}}", n.effects.join("; "))
                }
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
        if region.runtime_checks > 0 {
            out += &format!(
                "    paths compared at run time: {} pair(s) of chains wait for each other if \
                 their files overlap\n",
                region.runtime_checks
            );
        }
    }
    for l in &report.loops {
        let what = if l.changed {
            let check = if l.check.is_empty() {
                String::new()
            } else {
                format!("; per-item files {}", l.check)
            };
            format!(
                "prefetch [{}], {} at a time{check}",
                l.prefetched.join(", "),
                l.bound
            )
        } else {
            l.reason.clone()
        };
        out += &format!("  loop `{}`: {what}\n", l.label);
    }
    let mut unclassified: Vec<String> = Vec::new();
    for region in &report.regions {
        for (_, call) in &region.unclassified {
            if !unclassified.contains(call) {
                unclassified.push(call.clone());
            }
        }
    }
    if !unclassified.is_empty() {
        out += &format!(
            "  unclassified (assumed to read; `strict` treats them as writes): {}\n",
            unclassified.join(", ")
        );
    }
    for b in &report.barriers {
        out += &format!("  barrier: {b}\n");
    }
    out
}

#[cfg(test)]
mod tests {
    /// The certificate is never more lenient than psync: every edge psync draws is a conflict
    /// between the two nodes' places, so `verify/Cert.lean` can only reject what psync accepted,
    /// not accept what psync would not. Checked over every annotated function of the graph
    /// tests, the driver fixture and `graph_suite`.
    /// Synchronous code (Phase II): a blocking call is classified by its verb as an awaited one is,
    /// so a delete and a get of one name keep their order; a receiver bound inside a statement is
    /// that statement's own, so two downloads over their own sockets do not conflict.
    #[test]
    fn synchronous_calls_have_effects() {
        let r = report(
            Options::default(),
            syn::parse_quote! {
                fn recreate(client: &Client, name: &str) -> Result<u32, E> {
                    #[psync::par]
                    client.delete(name)?;
                    #[psync::par]
                    let v = client.get(name)?;
                    Ok(v)
                }
            },
        );
        assert!(
            edges(&r).iter().any(|e| e.contains("delete")),
            "{:?}",
            edges(&r)
        );
        assert!(!r.regions[0].changed);
        let r = report(
            Options::default(),
            syn::parse_quote! {
                fn two(addr: Addr) -> std::io::Result<usize> {
                    let a = { let mut s = TcpStream::connect(addr)?; s.read_to_end(&mut v1)? };
                    let b = { let mut s = TcpStream::connect(addr)?; s.read_to_end(&mut v2)? };
                    Ok(a + b)
                }
            },
        );
        assert!(edges(&r).is_empty(), "{:?}", edges(&r));
        assert!(r.regions[0].changed);
    }

    /// rustic's `diff`: `let path2 = path2.unwrap_or(path1);` sits between the two lookups and feeds
    /// the second. `unwrap_or` was unclassified, so the statement was its own chain and delayed
    /// the second lookup by a level; with pure arguments it is pure.
    #[test]
    fn cheap_statements_do_not_delay_waits() {
        let mut f: ItemFn = syn::parse_quote! {
            fn diff(repo: &Repo, path1: Option<&str>, path2: Option<&str>) -> Result<()> {
                let snaps = repo.get_snapshots(&[1, 2])?;
                let snap1 = &snaps[0];
                let snap2 = &snaps[1];
                let path1 = path1.unwrap_or("");
                let path2 = path2.unwrap_or(path1);
                info!("{path1} {path2}");
                #[psync::par]
                let node1 = repo.node(snap1, path1)?;
                #[psync::par]
                let node2 = repo.node(snap2, path2)?;
                show(node1, node2)
            }
        };
        let report = transform_fn(&mut f, &Options::default());
        let r = &report.regions[0];
        let level_of = |name: &str| {
            let n = r
                .nodes
                .iter()
                .position(|n| n.label.starts_with(name))
                .unwrap();
            let c = r.chains.iter().position(|c| c.contains(&n)).unwrap();
            r.levels.iter().position(|l| l.contains(&c)).unwrap()
        };
        assert_eq!(level_of("node1"), level_of("node2"), "{:?}", r.levels);
    }

    /// tauri-bundler's `prepare_tools`: three cached downloads, with path joins and a choice
    /// between two string literals in between. Those bindings are pure, and a node whose edges all
    /// come from one chain joins that chain, so the three downloads start together.
    #[test]
    fn path_joins_and_pure_ifs_do_not_delay_downloads() {
        let mut f: ItemFn = syn::parse_quote! {
            fn prepare_tools(tools_path: &Path, arch: &str) -> Result<PathBuf> {
                let apprun = tools_path.join(format!("AppRun-{arch}"));
                #[psync::par]
                #[psync::independent]
                if !apprun.exists() {
                    let data = download("apprun")?;
                    write_and_make_executable(&apprun, &data)?;
                }
                let linuxdeploy_arch = if arch == "i686" { "i386" } else { arch };
                let linuxdeploy = tools_path.join(format!("linuxdeploy-{linuxdeploy_arch}"));
                #[psync::par]
                #[psync::independent]
                #[psync::speculate]
                if !linuxdeploy.exists() {
                    let data = download(&format!("linuxdeploy-{linuxdeploy_arch}"))?;
                    write_and_make_executable(&linuxdeploy, &data)?;
                }
                Ok(linuxdeploy)
            }
        };
        let report = transform_fn(&mut f, &Options::default());
        let r = &report.regions[0];
        let downloads: Vec<usize> = (0..r.nodes.len())
            .filter(|&n| r.nodes[n].label.contains("psync :: par"))
            .map(|n| {
                let c = r.chains.iter().position(|c| c.contains(&n)).unwrap();
                r.levels.iter().position(|l| l.contains(&c)).unwrap()
            })
            .collect();
        assert_eq!(downloads, [0, 0], "{:?}", r.levels);
    }

    /// `p.exists()` reads the file system at `p`: in synchronous code it must not overlap a write
    /// to `p` (it was a read of a service named `p`, which no file write conflicts with).
    #[test]
    fn path_exists_reads_the_file_system() {
        let mut f: ItemFn = syn::parse_quote! {
            fn f(p: &Path) -> std::io::Result<bool> {
                #[psync::par]
                let there = p.exists();
                #[psync::par]
                std::fs::write(p, "x")?;
                Ok(there)
            }
        };
        let report = transform_fn(&mut f, &Options::default());
        let r = &report.regions[0];
        assert!(
            r.edges.iter().any(|(a, b, _)| (*a, *b) == (0, 1)),
            "{:?}",
            r.levels
        );
    }

    /// `let a = dir.join("a.json")`: a write to `&a` is a write to the file `dir/a.json`, which a
    /// write to `dir/b-{x}` does not touch. A name that may be absolute (`/a`, a hole first, `C:`)
    /// or a base that may change (`mut`) leaves the target unknown.
    #[test]
    fn joined_paths_name_their_files() {
        fn levels(b: syn::Stmt, base_mut: bool) -> Vec<usize> {
            let first: syn::Stmt = if base_mut {
                syn::parse_quote!(let mut dir = root.to_path_buf();)
            } else {
                syn::parse_quote!(let dir = root;)
            };
            let mut f: ItemFn = syn::parse_quote! {
                fn f(root: &Path, x: &str) -> std::io::Result<()> {
                    #first
                    let a = dir.join("a.json");
                    #b
                    #[psync::par]
                    if !a.exists() {
                        std::fs::write(&a, fetch("a")?)?;
                    }
                    #[psync::par]
                    #[psync::speculate]
                    if !b.exists() {
                        std::fs::write(&b, fetch("b")?)?;
                    }
                    Ok(())
                }
            };
            let report = transform_fn(&mut f, &Options::default());
            let r = &report.regions[0];
            (0..r.nodes.len())
                .filter(|&n| r.nodes[n].label.contains("psync :: par"))
                .map(|n| {
                    let c = r.chains.iter().position(|c| c.contains(&n)).unwrap();
                    r.levels.iter().position(|l| l.contains(&c)).unwrap()
                })
                .collect()
        }
        let apart: syn::Stmt = syn::parse_quote!(let b = dir.join(format!("b-{x}")););
        let together = |l: Vec<usize>| l[0] == l[1];
        assert!(together(levels(apart.clone(), false)));
        assert!(!together(levels(apart, true)), "a `mut` base");
        for b in [
            syn::parse_quote!(let b = dir.join("a.json");),
            syn::parse_quote!(let b = dir.join(format!("{x}"));),
            syn::parse_quote!(let b = dir.join("/b");),
            syn::parse_quote!(let b = dir.join("C:b");),
            syn::parse_quote!(let mut b = dir.join("b");),
        ] {
            let shown = quote::quote!(#b).to_string();
            assert!(!together(levels(b, false)), "{shown}");
        }
    }

    /// `handle.join()` waits for a thread: unlike `path.join(name)`, it is not a pure value.
    #[test]
    fn thread_join_is_not_pure() {
        let e: syn::Expr = syn::parse_quote!(handle.join());
        assert!(!classify::is_pure(&e));
        let e: syn::Expr = syn::parse_quote!(dir.join("a"));
        assert!(classify::is_pure(&e));
        let e: syn::Expr = syn::parse_quote!(if a == 1 { "x" } else { b });
        assert!(classify::is_pure(&e));
        let e: syn::Expr = syn::parse_quote!(if a == 1 { f() } else { b });
        assert!(!classify::is_pure(&e));
    }

    /// A loop body may end in an await without `;` (`api.put(x).await`, as in DataFusion's join
    /// fuzz tests and greptimedb's `wait_datanodes_alive`): the generated code was invalid and the
    /// macro panicked. Found by `psync-scan` over the audit corpus.
    #[test]
    fn loop_bodies_ending_in_an_await_without_semicolon() {
        for f in [
            syn::parse_quote! {
                async fn each(api: &Api, xs: Vec<u32>) {
                    for x in xs {
                        api.put(x).await
                    }
                }
            },
            syn::parse_quote! {
                async fn poll(api: &Api) {
                    for _ in 0..100 {
                        let n = api.count().await.unwrap();
                        if n == 3 {
                            return;
                        }
                        tokio::time::sleep(D).await
                    }
                }
            },
        ] {
            let mut f: ItemFn = f;
            transform_fn(&mut f, &Options::default());
        }
    }

    /// `let x: u32 = ..` inside a statement binds `x`; it does not write `u32` (a false edge
    /// between every two statements with such an annotation).
    #[test]
    fn annotated_bindings_are_not_assignments() {
        let r = report(
            Options::default(),
            syn::parse_quote! {
                async fn f(api: &Api) -> u32 {
                    let a = { let x: u32 = api.one().await; x };
                    let b = { let y: u32 = api.two().await; y };
                    a + b
                }
            },
        );
        assert!(edges(&r).is_empty(), "{:?}", edges(&r));
        assert!(r.regions[0].changed);
    }

    #[test]
    fn certificates_cover_every_edge() {
        use proc_macro2::TokenTree;
        // As `overlap` in `verify/Cert.lean`: agree until one ends, or first differ at a hole.
        let overlap = |p: &Vec<String>, q: &Vec<String>| {
            p.iter()
                .zip(q)
                .find(|(a, b)| a != b)
                .is_none_or(|(a, b)| a.starts_with("$hole:") || b.starts_with("$hole:"))
        };
        let touches = |ws: &[Vec<String>], others: &[Vec<String>]| {
            ws.iter().any(|w| others.iter().any(|o| overlap(w, o)))
        };
        let conflict = |a: &CertNode, b: &CertNode| {
            let b_all: Vec<Vec<String>> = b.reads.iter().chain(&b.writes).cloned().collect();
            touches(&a.writes, &b_all) || touches(&b.writes, &a.reads)
        };
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
        let mut files = vec![
            format!("{root}/tests/graph.rs"),
            format!("{root}/tests/sync.rs"),
            format!("{root}/psync-check/tests/fixtures/hidden/app/src/lib.rs"),
        ];
        for f in std::fs::read_dir(format!("{root}/examples/graph_suite")).unwrap() {
            files.push(f.unwrap().path().display().to_string());
        }
        let is_graph = |attrs: &[syn::Attribute]| {
            attrs
                .iter()
                .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "graph"))
        };
        let (mut functions, mut checked) = (0, 0);
        for file in files {
            let parsed = syn::parse_file(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let mut fns: Vec<(Options, ItemFn)> = Vec::new();
            let opts = |attrs: &[syn::Attribute]| match attrs
                .iter()
                .find(|a| a.path().segments.last().is_some_and(|s| s.ident == "graph"))
                .map(|a| &a.meta)
            {
                Some(syn::Meta::List(l)) => Options::parse(l.tokens.clone()).unwrap(),
                _ => Options::default(),
            };
            for item in &parsed.items {
                match item {
                    syn::Item::Fn(f) if is_graph(&f.attrs) => fns.push((opts(&f.attrs), f.clone())),
                    syn::Item::Macro(m) if m.mac.path.is_ident("example") => {
                        let tts: Vec<TokenTree> = m.mac.tokens.clone().into_iter().collect();
                        let at = tts.windows(2).position(|w| {
                            matches!((&w[0], &w[1]), (TokenTree::Ident(a), TokenTree::Ident(f)) if a == "async" && f == "fn")
                        });
                        let options = tts
                            .windows(3)
                            .find_map(|w| match (&w[0], &w[2]) {
                                (TokenTree::Ident(i), TokenTree::Group(g)) if i == "options" => {
                                    Some(Options::parse(g.stream()).unwrap())
                                }
                                _ => None,
                            })
                            .unwrap_or_default();
                        if let Some(at) = at {
                            let body: TokenStream = tts[at..].iter().cloned().collect();
                            fns.push((options, syn::parse2(body).unwrap()));
                        }
                    }
                    _ => {}
                }
            }
            for (options, f) in fns {
                functions += 1;
                // Under its own options, and under the defaults (which keep more edges).
                let runs = [report(options, f.clone()), report(Options::default(), f)];
                for region in runs.iter().flat_map(|r| &r.regions) {
                    for (i, j, why) in &region.edges {
                        checked += 1;
                        assert!(
                            conflict(&region.cert[*i], &region.cert[*j]),
                            "{file}: edge n{i} -> n{j} ({why}) has no conflict in the certificate"
                        );
                    }
                }
            }
        }
        assert!(
            functions > 40 && checked > 120,
            "{functions} functions, {checked} edges"
        );
    }

    use super::*;

    fn report(options: Options, f: ItemFn) -> Report {
        let mut f = f;
        transform_fn(&mut f, &options)
    }

    fn edges(r: &Report) -> Vec<String> {
        r.regions
            .iter()
            .flat_map(|g| g.edges.iter().map(|e| e.2.clone()))
            .collect()
    }

    #[test]
    fn files_and_services_order_conflicting_awaits() {
        let r = report(
            Options::default(),
            syn::parse_quote! {
                async fn f(api: &Api) -> Result<(), E> {
                    tokio::fs::write("cfg.toml", b"x").await?;
                    let s = tokio::fs::read_to_string("cfg.toml").await?;
                    let o = tokio::fs::read_to_string("other.toml").await?;
                    api.delete(name).await?;
                    Ok(())
                }
            },
        );
        let e = edges(&r);
        assert!(
            e.iter().any(|w| w.starts_with("write file \"cfg.toml\"")),
            "{e:?}"
        );
        assert!(
            e.iter().any(|w| w.starts_with("may exit before `delete`")),
            "{e:?}"
        );
        // `other.toml` neither conflicts with the write nor is a write: it overlaps.
        assert!(r.regions[0].levels[0].len() >= 2);
    }

    #[test]
    fn literal_paths_are_compared_when_compiling() {
        let region = |f: ItemFn| {
            let r = report(Options::parse(quote!(speculate)).unwrap(), f);
            r.regions[0].clone()
        };
        // Two literals are compared exactly: a directory and a file inside it stay ordered by an
        // edge, with nothing left to compare at run time, and the labels keep their paths.
        let r = region(syn::parse_quote! {
            async fn f() -> std::io::Result<()> {
                tokio::fs::remove_dir_all("out").await?;
                let a = tokio::fs::read("out/a.txt").await?;
                let b = tokio::fs::read("cfg.toml").await?;
                Ok(())
            }
        });
        assert_eq!(r.runtime_checks, 0);
        assert!(
            r.edges.iter().any(|e| (e.0, e.1) == (0, 1)),
            "{:?}",
            r.edges
        );
        assert!(!format!("{:?}", r.nodes).contains("compared at run time"));
        // A literal against a computed path is still compared at run time.
        let r = region(syn::parse_quote! {
            async fn f(p: &Path) -> std::io::Result<()> {
                tokio::fs::remove_dir_all("out").await?;
                let a = tokio::fs::read(p).await?;
                Ok(())
            }
        });
        assert_eq!(r.runtime_checks, 1);
    }

    #[test]
    fn computed_paths_are_compared_at_run_time() {
        let checks = |f: ItemFn| {
            let r = report(Options::parse(quote!(speculate)).unwrap(), f);
            (r.regions[0].runtime_checks, r.regions[0].changed)
        };
        // Two parameters: concurrent, ordered at run time if they name one file.
        assert_eq!(
            checks(syn::parse_quote! {
                async fn f(a: &Path, b: &Path) -> Result<(), E> {
                    let s = tokio::fs::read_to_string(a).await?;
                    tokio::fs::write(b, b"x").await?;
                    Ok(())
                }
            }),
            (1, true)
        );
        // A path bound in the region to a total value is substituted.
        assert_eq!(
            checks(syn::parse_quote! {
                async fn f(dir: &Path, name: &str) -> Result<(), E> {
                    let p = dir.join(name);
                    let s = tokio::fs::read_to_string(&p).await?;
                    tokio::fs::write(dir.join("out"), b"x").await?;
                    Ok(())
                }
            })
            .0,
            1
        );
        // The same expression is the same file: it stays ordered.
        let same = |f: ItemFn| assert_eq!(checks(f).0, 0);
        same(syn::parse_quote! {
            async fn f(a: &Path) -> Result<(), E> {
                let s = tokio::fs::read_to_string(a).await?;
                tokio::fs::write(a, b"x").await?;
                Ok(())
            }
        });
        // A path from awaited work, or from a `mut` binding, cannot be evaluated at the start.
        same(syn::parse_quote! {
            async fn f(api: &Api, a: &Path) -> Result<(), E> {
                let b = api.locate().await?;
                let s = tokio::fs::read_to_string(a).await?;
                tokio::fs::write(&b, b"x").await?;
                Ok(())
            }
        });
        same(syn::parse_quote! {
            async fn f(a: &Path, mut b: PathBuf) -> Result<(), E> {
                let s = tokio::fs::read_to_string(a).await?;
                tokio::fs::write(&b, b"x").await?;
                Ok(())
            }
        });
    }

    #[test]
    fn per_item_files_are_checked_at_run_time() {
        let check = |f: ItemFn| {
            let r = report(Options::parse(quote!(speculate)).unwrap(), f);
            assert!(r.loops[0].changed, "{:?}", r.loops);
            r.loops[0].check.clone()
        };
        // The path is computed from the item: evaluated ahead for every item.
        let checked = |c: String| assert_eq!(c, "checked at run time");
        checked(check(syn::parse_quote! {
            async fn f(dir: &Path, names: &[String]) -> Result<(), E> {
                for name in names {
                    let path = dir.join(name);
                    let old = tokio::fs::read_to_string(&path).await?;
                    std::fs::write(&path, old)?;
                }
                Ok(())
            }
        }));
        checked(check(syn::parse_quote! {
            async fn f(names: &[String]) -> Result<(), E> {
                for (i, name) in names.iter().enumerate() {
                    tokio::fs::write(format!("{name}.json"), b"x").await?;
                }
                Ok(())
            }
        }));
        // Only reads: nothing to check.
        assert_eq!(
            check(syn::parse_quote! {
                async fn f(names: &[String]) -> Result<(), E> {
                    for name in names {
                        let s = tokio::fs::read_to_string(format!("{name}.json")).await?;
                    }
                    Ok(())
                }
            }),
            ""
        );
        // Paths that cannot be evaluated ahead keep the assumption, and say why.
        let assumed = |c: String, why: &str| {
            assert!(c.starts_with("assumed distinct") && c.contains(why), "{c}")
        };
        assumed(
            check(syn::parse_quote! {
                async fn f(api: &Api, names: &[String]) -> Result<(), E> {
                    for name in names {
                        let path = api.locate(name).await?;
                        tokio::fs::write(&path, b"x").await?;
                    }
                    Ok(())
                }
            }),
            "computed in the body",
        );
        assumed(
            check(syn::parse_quote! {
                async fn f(paths: &[PathBuf]) -> Result<(), E> {
                    for i in 0..paths.len() {
                        tokio::fs::write(&paths[i], b"x").await?;
                    }
                    Ok(())
                }
            }),
            "may panic",
        );
        assumed(
            check(syn::parse_quote! {
                async fn f(pairs: Vec<(String, u8)>) -> Result<(), E> {
                    for (mut name, n) in pairs {
                        tokio::fs::write(&name, [n]).await?;
                    }
                    Ok(())
                }
            }),
            "`mut`",
        );
        // `independent` is the programmer's word for it.
        assert_eq!(
            check(syn::parse_quote! {
                async fn f(names: &[String]) -> Result<(), E> {
                    #[psync::independent]
                    for name in names {
                        tokio::fs::write(format!("{name}.json"), b"x").await?;
                    }
                    Ok(())
                }
            }),
            ""
        );
    }

    #[test]
    fn loop_writes_past_exits_are_refused_unless_speculative() {
        let f: ItemFn = syn::parse_quote! {
            async fn f(store: &Store, keys: &[String]) -> Result<(), E> {
                for k in keys {
                    store.upload(k).await?;
                }
                Ok(())
            }
        };
        let r = report(Options::default(), f.clone());
        assert!(
            !r.loops[0].changed && r.loops[0].reason.contains("speculate"),
            "{:?}",
            r.loops
        );
        let options = Options::parse(quote!(speculate)).unwrap();
        assert!(report(options, f).loops[0].changed);
    }

    #[test]
    fn same_target_every_iteration_is_refused() {
        let r = report(
            Options::default(),
            syn::parse_quote! {
                async fn f(store: &Store, keys: &[String]) {
                    for k in keys {
                        let _ = store.put("counter", k).await;
                    }
                }
            },
        );
        assert!(
            r.loops[0].reason.contains("every iteration"),
            "{:?}",
            r.loops
        );
    }

    #[test]
    fn effect_options_parse() {
        let o = Options::parse(quote!(
            strict,
            effects(write(bump), read(peek_all), ordered(log_line))
        ))
        .unwrap();
        assert!(
            o.effects.strict && o.effects.writes == ["bump"] && o.effects.ordered == ["log_line"]
        );
        assert!(Options::parse(quote!(effects(maybe(x)))).is_err());
    }
}

/// `#[psync::observed]`: the item unchanged, and a marker `const __PSYNC_OBSERVED_<NAME>` beside
/// it for `cargo psync`'s driver. The item's tokens are not parsed (inside `thread_local!` they
/// hold the standard library's internal macros): its name is the identifier after `static` or
/// `const` (and `mut`).
pub fn observed(item: TokenStream) -> syn::Result<TokenStream> {
    use proc_macro2::TokenTree;
    let tokens: Vec<TokenTree> = item.clone().into_iter().collect();
    let name = tokens
        .iter()
        .enumerate()
        .find_map(|(i, t)| match t {
            TokenTree::Ident(k) if k == "static" || k == "const" => {
                tokens[i + 1..].iter().find_map(|t| match t {
                    TokenTree::Ident(n) if n != "mut" => Some(n.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[psync::observed] goes on a `static` (or one inside `thread_local!`)",
            )
        })?;
    let marker = quote::format_ident!("__PSYNC_OBSERVED_{}", name);
    Ok(quote! {
        #item
        #[doc(hidden)]
        #[allow(dead_code, non_upper_case_globals)]
        const #marker: () = ();
    })
}
