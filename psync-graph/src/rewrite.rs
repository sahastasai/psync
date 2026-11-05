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
}

impl VisitMut for Rewriter<'_> {
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

    // Nested functions are separate code; they need their own attribute.
    fn visit_item_mut(&mut self, _: &mut syn::Item) {}

    fn visit_block_mut(&mut self, block: &mut Block) {
        let mut stmts = Vec::with_capacity(block.stmts.len());
        for mut stmt in std::mem::take(&mut block.stmts) {
            match &mut stmt {
                // Loops are rewritten from their original bodies (the loop code visits the parts
                // it keeps); other statements have their inner blocks rewritten first.
                Stmt::Expr(Expr::ForLoop(f), semi) if self.in_async => {
                    self.visit_expr_mut(&mut f.expr);
                    if let Some(rewritten) = self.rewrite_loop(f) {
                        stmts.push(Stmt::Expr(rewritten, *semi));
                        continue;
                    }
                    self.visit_block_mut(&mut f.body);
                }
                Stmt::Expr(Expr::While(w), semi) if self.in_async => {
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
    /// argument, a method receiver) into a binding of the awaited value and the rest of the
    /// statement, repeatedly, since a hoisted value may itself hide an await.
    fn expose_awaits(&mut self, stmts: Vec<Stmt>) -> Vec<Stmt> {
        let mut out = Vec::with_capacity(stmts.len());
        let mut work: Vec<(Stmt, usize)> = stmts.into_iter().rev().map(|s| (s, 0)).collect();
        while let Some((stmt, depth)) = work.pop() {
            let hides_await = match classify(&stmt, &self.scope, false) {
                Class::Node(n) => n.effect && n.awaits,
                Class::Barrier(_) => Exits::of_stmt(&stmt).awaits,
            };
            if hides_await && depth < MAX_HOIST_DEPTH {
                let n = self.next();
                let split = hoist::scrutinee(&stmt, n)
                    .map(|(value, rest)| (vec![value], rest))
                    .or_else(|| hoist::operands(&stmt, &mut self.fresh));
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
        let plan = plan(&nodes);
        self.report.regions.push(plan.report.clone());
        if !plan.report.changed {
            return nodes.into_iter().map(|n| n.stmt).collect();
        }
        emit(
            &self.options.krate,
            &self.scope.fresh,
            nodes,
            &plan,
            &positions,
            mentions,
        )
    }
}
