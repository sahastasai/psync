//! The rewriting pass.
//!
//! [`Rewriter`] walks the function with `syn`'s `VisitMut`, innermost blocks first. In every block
//! of async code it
//!
//! 1. hands `for` and `while` loops to [`crate::loops`],
//! 2. exposes awaits hidden inside larger statements ([`Rewriter::expose_awaits`]),
//! 3. splits the block into regions at barriers and schedules each region
//!    ([`crate::schedule`]).
//!
//! A block whose regions all stay sequential is emitted exactly as written.

use crate::access::{Set, mentions};
use crate::classify::{Class, Exits, Node, Scope, classify, line_of};
use crate::effects::{Target, value_target};
use crate::schedule::{emit, plan};
use crate::{Options, Report, hoist};
use quote::{ToTokens, format_ident};
use syn::{Block, Expr, Stmt, spanned::Spanned, visit_mut::VisitMut};

/// How many times a statement may be split while exposing nested awaits.
const MAX_HOIST_DEPTH: usize = 8;

/// The state of one `#[psync::graph]` expansion.
pub(crate) struct Rewriter<'a> {
    pub options: &'a Options,
    /// Mutability facts and hints for the whole function.
    pub scope: Scope,
    /// Everything analyzed so far, for `explain` and tooling.
    pub report: Report,
    /// Inside an `async fn`, async block or async closure: only there do awaits occur.
    pub in_async: bool,
    /// Counter that keeps generated names unique within the function.
    pub fresh: usize,
    /// Why the last loop statement could not run ahead, for the loop's report.
    pub refusal: Option<String>,
    /// How the last prefetched loop's per-item files are kept apart, for its report.
    pub loop_check: String,
}

impl VisitMut for Rewriter<'_> {
    fn visit_expr_async_mut(&mut self, e: &mut syn::ExprAsync) {
        // Inside an async block, even one in a synchronous function, awaits are what overlap.
        let outer = std::mem::replace(&mut self.in_async, true);
        let outer_sync = std::mem::replace(&mut self.scope.sync, false);
        syn::visit_mut::visit_expr_async_mut(self, e);
        self.in_async = outer;
        self.scope.sync = outer_sync;
    }

    fn visit_expr_closure_mut(&mut self, e: &mut syn::ExprClosure) {
        let outer = std::mem::replace(&mut self.in_async, e.asyncness.is_some());
        let outer_sync = std::mem::replace(&mut self.scope.sync, false);
        syn::visit_mut::visit_expr_closure_mut(self, e);
        self.in_async = outer;
        self.scope.sync = outer_sync;
    }

    // Nested functions are separate code; they need their own attribute.
    fn visit_item_mut(&mut self, _: &mut syn::Item) {}

    fn visit_block_mut(&mut self, block: &mut Block) {
        let mut stmts = Vec::with_capacity(block.stmts.len());
        for mut stmt in std::mem::take(&mut block.stmts) {
            match &mut stmt {
                // Loops are rewritten from their original bodies (the loop code visits the parts
                // it keeps); other statements have their inner blocks rewritten first.
                // Loops of synchronous code are not prefetched yet.
                Stmt::Expr(Expr::ForLoop(f), semi) if self.in_async && !self.scope.sync => {
                    self.visit_expr_mut(&mut f.expr);
                    if let Some(rewritten) = self.rewrite_loop(f) {
                        stmts.push(Stmt::Expr(rewritten, *semi));
                        continue;
                    }
                    self.visit_block_mut(&mut f.body);
                }
                Stmt::Expr(Expr::While(w), semi) if self.in_async && !self.scope.sync => {
                    if let Some(rewritten) = self.rewrite_while(w) {
                        stmts.push(Stmt::Expr(rewritten, *semi));
                        continue;
                    }
                    self.visit_stmt_mut(&mut stmt);
                }
                _ => self.visit_stmt_mut(&mut stmt),
            }
            stmts.push(stmt);
        }
        block.stmts = if self.in_async {
            self.rewrite_stmts(stmts)
        } else {
            stmts
        };
    }
}

impl Rewriter<'_> {
    /// A fresh number for generated names.
    pub(crate) fn next(&mut self) -> usize {
        self.fresh += 1;
        self.fresh
    }

    /// Rewrite one block's statements, or return them unchanged if nothing becomes concurrent.
    fn rewrite_stmts(&mut self, stmts: Vec<Stmt>) -> Vec<Stmt> {
        let tail_hoisted = self.hoist_tail(stmts.clone());
        let exposed = self.expose_awaits(tail_hoisted);
        let before = self.report.regions.len();
        let out = self.schedule_block(exposed);
        if self.report.regions[before..].iter().any(|r| r.changed) {
            out
        } else {
            stmts
        }
    }

    /// An awaited block value (`foo().await?` last) becomes `let t = foo().await?; t`, so the await
    /// can overlap earlier work while the value still comes last.
    fn hoist_tail(&mut self, mut stmts: Vec<Stmt>) -> Vec<Stmt> {
        let Some(Stmt::Expr(tail, None)) = stmts.last() else {
            return stmts;
        };
        let as_statement = Stmt::Expr(tail.clone(), Some(Default::default()));
        let movable =
            matches!(classify(&as_statement, &self.scope, false), Class::Node(ref n) if !n.effect);
        if Exits::of(tail).awaits && movable {
            let value = tail.clone();
            let name = format_ident!("__psync_t{}", self.next());
            stmts.pop();
            stmts.push(syn::parse_quote_spanned!(value.span()=> let #name = #value;));
            stmts.push(Stmt::Expr(syn::parse_quote!(#name), None));
        }
        stmts
    }

    /// Split statements that hide an await inside other work (a `match` scrutinee, a call
    /// argument, a method receiver, the leading work of an `if` branch) into a binding of the
    /// awaited value and the rest of the statement, repeatedly, since a hoisted value may itself
    /// hide an await.
    fn expose_awaits(&mut self, stmts: Vec<Stmt>) -> Vec<Stmt> {
        let mut out = Vec::with_capacity(stmts.len());
        let mut work: Vec<(Stmt, usize)> = stmts.into_iter().rev().map(|s| (s, 0)).collect();
        while let Some((stmt, depth)) = work.pop() {
            let hides_await = match classify(&stmt, &self.scope, false) {
                // A binding hides an await when its value only contains one: in
                // `let t = combine(a, f().await);` the await need not wait for `a`.
                Class::Node(n) => n.awaits && (n.effect || !awaited_value(&stmt)),
                Class::Barrier(_) => Exits::of_stmt(&stmt).awaits,
            };
            if hides_await && depth < MAX_HOIST_DEPTH {
                let n = self.next();
                let split = hoist::scrutinee(&stmt, n)
                    .map(|(value, rest)| (vec![value], rest))
                    .or_else(|| hoist::operands(&stmt, &mut self.fresh))
                    .or_else(|| self.hoist_branch(&stmt).map(|(g, rest)| (vec![g], rest)));
                if let Some((values, rest)) = split {
                    work.push((rest, depth + 1));
                    work.extend(values.into_iter().rev().map(|v| (v, depth + 1)));
                    continue;
                }
            }
            out.push(stmt);
        }
        out
    }

    /// Split a block into regions at barriers and schedule each region.
    fn schedule_block(&mut self, stmts: Vec<Stmt>) -> Vec<Stmt> {
        let mentions: Vec<Set> = stmts
            .iter()
            .map(|s| mentions(s.to_token_stream()))
            .collect();
        let mut out = Vec::new();
        let mut region: Vec<(usize, Node)> = Vec::new();
        let last = stmts.len().saturating_sub(1);
        // What the block's immutable bindings name, for effects whose target is a binding:
        // `let p = dir.join("a.json"); .. fs::write(&p, ..)` writes the file `dir/a.json`.
        let mut values: std::collections::HashMap<String, Target> = Default::default();
        for (index, stmt) in stmts.into_iter().enumerate() {
            // The block's final expression is its value, so it stays last.
            let class = if index == last && matches!(stmt, Stmt::Expr(_, None)) {
                let awaits = Exits::of_stmt(&stmt).awaits;
                Class::Barrier(
                    awaits.then(|| format!("line {}: the block's value", line_of(stmt.span()))),
                )
            } else {
                classify(&stmt, &self.scope, false)
            };
            let class = match class {
                Class::Node(mut n) => {
                    let inner = crate::classify::inner_bindings(&n.stmt);
                    for e in &mut n.effects {
                        if let Some(name) = e.target.single_name()
                            && !inner.contains(name)
                            && let Some(t) = values.get(name)
                        {
                            e.target = t.clone();
                        }
                    }
                    Class::Node(n)
                }
                barrier => barrier,
            };
            bind_values(&mut values, &stmt, &self.scope.mutable);
            match class {
                Class::Node(n) => region.push((index, *n)),
                Class::Barrier(why) => {
                    // Generated code (already-rewritten loops) is not worth reporting again.
                    if let Some(why) = why.filter(|w| !w.contains("__psync")) {
                        self.report.barriers.push(why);
                    }
                    out.extend(self.schedule_region(std::mem::take(&mut region), &mentions));
                    out.push(stmt);
                }
            }
        }
        out.extend(self.schedule_region(region, &mentions));
        out
    }

    /// Schedule one region; a region with fewer than two awaits has nothing to overlap.
    fn schedule_region(&mut self, region: Vec<(usize, Node)>, mentions: &[Set]) -> Vec<Stmt> {
        if region.iter().filter(|(_, n)| n.awaits).count() < 2 {
            return region.into_iter().map(|(_, n)| n.stmt).collect();
        }
        let (positions, nodes): (Vec<usize>, Vec<Node>) = region.into_iter().unzip();
        // Computed paths the tokens cannot tell apart are compared at run time instead.
        let relaxed = (!self.scope.sync)
            .then(|| crate::gates::relax(&nodes, &self.scope.mutable, &mut self.fresh))
            .flatten();
        let (plan, gates, prelude) = match relaxed {
            Some((plan, gates, prelude)) => (plan, gates, prelude),
            None => (plan(&nodes), Vec::new(), Vec::new()),
        };
        let mut report = plan.report.clone();
        report.runtime_checks = gates.len();
        self.report.regions.push(report);
        if !plan.report.changed {
            return nodes.into_iter().map(|n| n.stmt).collect();
        }
        let mut out = prelude;
        out.extend(emit(
            self.scope.sync,
            &self.options.krate,
            &self.scope.fresh,
            nodes,
            &plan,
            &positions,
            mentions,
            &mut self.fresh,
            &gates,
        ));
        out
    }
}

/// Record what `stmt`'s bindings name. A `let` of one immutable name binds the target its value
/// names, if that says more than "anything" and depends on no `mut` binding (so it is the same
/// value wherever the name is used). Any binding forgets what was known of its name, and of the
/// values computed from it (they were computed from the shadowed one).
fn bind_values(values: &mut std::collections::HashMap<String, Target>, stmt: &Stmt, mutable: &Set) {
    let Stmt::Local(local) = stmt else {
        return;
    };
    let mut bound = Vec::new();
    crate::classify::pat_idents(&local.pat, &mut bound);
    for (name, _) in &bound {
        let name = name.to_string();
        values.remove(&name);
        values.retain(|_, t| t.names().is_none_or(|n| !n.contains(&name)));
    }
    let syn::Pat::Ident(p) = &local.pat else {
        return;
    };
    let Some(init) = &local.init else {
        return;
    };
    if p.mutability.is_some() || p.by_ref.is_some() || p.subpat.is_some() || init.diverge.is_some()
    {
        return;
    }
    let t = value_target(&init.expr);
    if t.informative() && t.names().is_none_or(|n| n.is_disjoint(mutable)) {
        values.insert(p.ident.to_string(), t);
    }
}

/// Whether a `let`'s value is itself awaited (`x.await`, `x.await?`, `x.await.len()`), so the
/// binding is already a node of its own await and there is nothing to expose.
fn awaited_value(stmt: &Stmt) -> bool {
    let Stmt::Local(syn::Local {
        init: Some(init), ..
    }) = stmt
    else {
        return true;
    };
    let mut e = &*init.expr;
    loop {
        match e {
            Expr::Await(_) => return true,
            Expr::Try(t) => e = &t.expr,
            Expr::Paren(p) => e = &p.expr,
            Expr::MethodCall(m) => e = &m.receiver,
            Expr::Field(f) => e = &f.base,
            _ => return false,
        }
    }
}
