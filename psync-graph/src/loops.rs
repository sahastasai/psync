//! Running loop iterations ahead.
//!
//! **Prefetch.** When the awaited statements at the front of a `for` loop body (its *prefix*) only
//! touch per-iteration data, psync starts the prefixes of up to `bound` later iterations while the
//! rest of the body still runs in order:
//!
//! ```text
//! for x in xs { let y = fetch(x).await?; out.push(y); }
//! // becomes
//! let items = prefetch(xs, bound, async |x| { <prefix, exits recorded> }).await;
//! for (exits.., bindings) in items { <re-raise exits>; let (x, y) = bindings; out.push(y); }
//! ```
//!
//! **Guards.** `if c { .. }` around awaited work becomes a prefix binding
//! `let g = if c { Some(<work>) } else { None };`, and an early exit `if c { ..; continue; }`
//! followed by awaited work becomes `let g = if !(c) { Some(<work>) } else { None };` (with `c`
//! pure), so the work runs ahead exactly for the items that reach it.
//!
//! **Fission.** When the body starts with statements that update loop-carried state without
//! awaiting (dedupe sets, map removals, permit reservations), those run first for every item in a
//! plain loop, and the remaining body is prefetched. Exits in that first loop that would end the
//! whole loop (`return`, `?`, `break` to an outer loop) are deferred until the items before them
//! have been processed, which is where sequential code performs them.

use crate::access::{Access, Set, conflict, mentions, root, scan};
use crate::classify::{
    Class, Exits, Node, bindings_in, classify, is_psync_attr, line_of, pat_has_ref, pat_idents,
};
use crate::exits::ExitRewriter;
use crate::rewrite::Rewriter;
use crate::schedule::chain_code;
use crate::{LoopReport, hoist};
use proc_macro2::{Ident, TokenStream};
use quote::{ToTokens, format_ident, quote};
use syn::{Block, Expr, ExprForLoop, Stmt, spanned::Spanned, visit_mut::VisitMut};

/// What a loop's prefix may touch.
struct LoopScope {
    /// Per-iteration names: the loop pattern and everything bound inside the body.
    local: Set,
    /// Places the body writes that outlive an iteration.
    carried: Set,
    /// `#[psync::independent]`: the programmer asserts iterations do not depend on each other.
    independent: bool,
}

impl LoopScope {
    fn new(
        rw: &Rewriter,
        pattern: &[(Ident, bool)],
        body: &Block,
        attrs: &[syn::Attribute],
    ) -> Self {
        let mut local: Set = pattern.iter().map(|(i, _)| i.to_string()).collect();
        local.extend(bindings_in(body));
        let mut acc = Access::default();
        scan(
            body.to_token_stream(),
            &rw.scope.mutable,
            &rw.scope.handles,
            &mut acc,
        );
        let carried = acc
            .wpaths
            .into_iter()
            .filter(|p| !is_local(&local, p))
            .collect();
        let independent = attrs.iter().any(|a| is_psync_attr(a, "independent"));
        Self {
            local,
            carried,
            independent,
        }
    }

    /// Why `n` cannot run ahead of earlier iterations, if it cannot.
    fn refuses(&self, n: &Node) -> Option<String> {
        if self.independent {
            return None;
        }
        if let Some(w) = n.acc.wpaths.iter().find(|p| !is_local(&self.local, p)) {
            return Some(format!("writes `{w}`, which outlives the iteration"));
        }
        conflict(&self.carried, &n.acc.rpaths)
            .map(|c| format!("reads `{c}`, which the loop body writes"))
    }
}

fn is_local(local: &Set, path: &str) -> bool {
    local.contains(root(path)) || root(path).starts_with("__psync")
}

/// `for a in xs { for b in f(a) { body } }` as `for (a, b) in <pairs> { body }`, when that is the
/// same loop: the outer body is only the inner loop, neither loop is labeled, `body` has no
/// `break` (which would end only the inner loop), and the outer item is a plain binding (it is
/// paired with every inner item, so it must be `Copy`, such as a reference; otherwise the
/// generated code does not compile rather than changing meaning).
fn flatten(f: &ExprForLoop) -> Option<ExprForLoop> {
    let [Stmt::Expr(Expr::ForLoop(inner), _)] = &f.body.stmts[..] else {
        return None;
    };
    let syn::Pat::Ident(outer) = &*f.pat else {
        return None;
    };
    if f.label.is_some() || inner.label.is_some() || outer.by_ref.is_some() || breaks(&inner.body) {
        return None;
    }
    let (a, xs, b, ys, body) = (&outer.ident, &f.expr, &inner.pat, &inner.expr, &inner.body);
    let attrs = &f.attrs;
    let pairs: Expr = syn::parse_quote! {
        ::core::iter::IntoIterator::into_iter(#xs).flat_map(move |#a| {
            ::core::iter::IntoIterator::into_iter(#ys).map(move |__psync_inner| (#a, __psync_inner))
        })
    };
    // Bind the outer item only if the body uses it.
    let outer_pat = if mentions(body.to_token_stream()).contains(&a.to_string()) {
        quote!(#a)
    } else {
        quote!(_)
    };
    Some(syn::parse_quote!(#(#attrs)* for (#outer_pat, #b) in #pairs #body))
}

/// True when `body` contains a `break` that would leave it (not one inside a nested loop).
fn breaks(body: &Block) -> bool {
    struct V(usize, bool);
    impl<'ast> syn::visit::Visit<'ast> for V {
        fn visit_expr_break(&mut self, _: &'ast syn::ExprBreak) {
            self.1 |= self.0 == 0;
        }
        fn visit_expr_for_loop(&mut self, e: &'ast ExprForLoop) {
            self.0 += 1;
            syn::visit::visit_expr_for_loop(self, e);
            self.0 -= 1;
        }
        fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
            self.0 += 1;
            syn::visit::visit_expr_while(self, e);
            self.0 -= 1;
        }
        fn visit_expr_loop(&mut self, e: &'ast syn::ExprLoop) {
            self.0 += 1;
            syn::visit::visit_expr_loop(self, e);
            self.0 -= 1;
        }
        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    }
    let mut v = V(0, false);
    syn::visit::Visit::visit_block(&mut v, body);
    v.1
}

/// The condition of `if c { ..; continue | break | return .. }` (no `else`) when `c` is pure.
fn exit_guard(stmt: &Stmt) -> Option<&Expr> {
    let Stmt::Expr(Expr::If(i), _) = stmt else {
        return None;
    };
    let ends_in_exit = matches!(
        i.then_branch.stmts.last(),
        Some(Stmt::Expr(
            Expr::Continue(_) | Expr::Break(_) | Expr::Return(_),
            _
        ))
    );
    (i.else_branch.is_none()
        && ends_in_exit
        && !matches!(&*i.cond, Expr::Let(_))
        && crate::classify::is_pure(&i.cond))
    .then_some(&*i.cond)
}

fn describe(f: &ExprForLoop) -> String {
    format!(
        "for {} in {}",
        f.pat.to_token_stream(),
        f.expr.to_token_stream()
    )
    .chars()
    .take(60)
    .collect()
}

impl Rewriter<'_> {
    /// Rewrite a `for` loop by prefetching or by fission then prefetching; `None` (recorded in
    /// the report) leaves it as written.
    pub(crate) fn rewrite_loop(&mut self, f: &ExprForLoop) -> Option<Expr> {
        let (label, line, end_line) = (
            describe(f),
            line_of(f.for_token.span()),
            crate::classify::end_line_of(f.span()),
        );
        // A loop whose body is only another loop is treated as one loop over the pairs, so the
        // awaited work of later outer items can run ahead too.
        let flat = flatten(f);
        let f = flat.as_ref().unwrap_or(f);
        let outcome = self.prefetch(f).or_else(|why| self.fission(f).ok_or(why));
        match outcome {
            Ok((expr, prefetched)) => {
                let bound = self.options.bound;
                self.report.loops.push(LoopReport {
                    label,
                    line,
                    end_line,
                    prefetched,
                    bound,
                    changed: true,
                    reason: String::new(),
                });
                Some(expr)
            }
            Err(reason) => {
                self.report.loops.push(LoopReport {
                    label,
                    line,
                    end_line,
                    reason,
                    ..Default::default()
                });
                None
            }
        }
    }

    /// Rewrite a `while`/`while let` loop by fission. Only with `#[psync::independent]`: the
    /// condition must be driven by the statements before the first await, which tokens cannot show.
    pub(crate) fn rewrite_while(&mut self, w: &syn::ExprWhile) -> Option<Expr> {
        if !w.attrs.iter().any(|a| is_psync_attr(a, "independent")) {
            return None;
        }
        let cond = &w.cond;
        let head = quote!(while #cond);
        let label = format!("while {}", cond.to_token_stream())
            .chars()
            .take(60)
            .collect();
        let (line, end_line) = (
            line_of(w.while_token.span()),
            crate::classify::end_line_of(w.span()),
        );
        match self.fission_with(head, &w.label, &w.attrs, &[], &w.body) {
            Some((expr, prefetched)) => {
                let bound = self.options.bound;
                self.report.loops.push(LoopReport {
                    label,
                    line,
                    end_line,
                    prefetched,
                    bound,
                    changed: true,
                    reason: String::new(),
                });
                Some(expr)
            }
            None => {
                let reason = "no awaited work can run ahead after the statements before it".into();
                self.report.loops.push(LoopReport {
                    label,
                    line,
                    end_line,
                    reason,
                    ..Default::default()
                });
                None
            }
        }
    }

    /// Prefetch the loop's awaited prefix. Returns the replacement and the prefetched labels.
    fn prefetch(&mut self, f: &ExprForLoop) -> Result<(Expr, Vec<String>), String> {
        if pat_has_ref(&f.pat) {
            return Err("`ref` in the loop pattern".into());
        }
        let mut pattern = Vec::new();
        pat_idents(&f.pat, &mut pattern);
        let scope = LoopScope::new(self, &pattern, &f.body, &f.attrs);
        let (prefix, rest) = self.take_prefix(f.body.stmts.clone(), true, &scope);
        if prefix.is_empty() {
            return Err("no awaited statement can run ahead".into());
        }
        // The prefix runs as one chain per item; its bindings flow into the rest of the body.
        let krate = self.options.krate.clone();
        let rest_mentions: Set = rest
            .iter()
            .flat_map(|s| mentions(s.to_token_stream()))
            .collect();
        let members: Vec<&Node> = prefix.iter().collect();
        let k = self.next();
        let code = chain_code(
            &krate,
            &self.scope.fresh,
            &members,
            &rest_mentions,
            k,
            &pattern,
        );
        let mut rest_block = Block {
            brace_token: f.body.brace_token,
            stmts: rest,
        };
        self.visit_block_mut(&mut rest_block);

        let (pat, iter, bound) = (&f.pat, &f.expr, self.options.bound);
        let attrs = f.attrs.iter().filter(|a| !is_psync_attr(a, "independent"));
        let loop_label = &f.label;
        let (chain, slot_idents, bindings) =
            (&code.body, code.slot_idents(), code.binding_pattern());
        let reraise = code.slots.iter().map(|s| s.reraise(&krate));
        let rest_stmts = &rest_block.stmts;
        let tokens = quote! {{
            let __psync_items = #krate::__private::prefetch(#iter, #bound, async |#pat| { #chain }).await;
            #(#attrs)* #loop_label for (#(#slot_idents,)* __psync_out) in __psync_items {
                #(#reraise)*
                #[allow(unused_mut)] let #bindings = __psync_out.unwrap();
                #(#rest_stmts)*
            }
        }};
        let prefetched = prefix.iter().map(|n| n.label.clone()).collect();
        Ok((
            syn::parse2(tokens).expect("psync::graph generated an invalid loop"),
            prefetched,
        ))
    }

    /// Split off the leading non-awaiting statements into a first loop, then prefetch the rest.
    fn fission(&mut self, f: &ExprForLoop) -> Option<(Expr, Vec<String>)> {
        if pat_has_ref(&f.pat) {
            return None;
        }
        let (pat, iter) = (&f.pat, &f.expr);
        let mut pattern = Vec::new();
        pat_idents(&f.pat, &mut pattern);
        self.fission_with(
            quote!(for #pat in #iter),
            &f.label,
            &f.attrs,
            &pattern,
            &f.body,
        )
    }

    /// Fission for a loop with the given header (`for p in e` or `while c`).
    fn fission_with(
        &mut self,
        head: TokenStream,
        loop_label: &Option<syn::Label>,
        attrs: &[syn::Attribute],
        pattern: &[(Ident, bool)],
        body: &Block,
    ) -> Option<(Expr, Vec<String>)> {
        // The first loop takes every statement before the first await; a leading `if`/`match`
        // whose branches await contributes just its condition.
        let mut stmts = body.stmts.clone();
        let mut first = Vec::new();
        let mut at = 0;
        while at < stmts.len() {
            let x = Exits::of_stmt(&stmts[at]);
            if x.blocker.is_some() {
                return None;
            }
            if x.awaits {
                let n = self.next();
                if let Some((cond, rewritten)) = hoist::condition(&stmts[at], n) {
                    first.push(cond);
                    stmts[at] = rewritten;
                }
                break;
            }
            first.push(stmts[at].clone());
            at += 1;
        }
        let rest: Vec<Stmt> = stmts.split_off(at);
        if first.is_empty() || rest.is_empty() {
            return None;
        }
        // The first loop now runs before the rest of every iteration, so the two parts must not
        // touch the same carried state (per-iteration names flow from one to the other).
        let mut local: Set = pattern.iter().map(|(i, _)| i.to_string()).collect();
        local.extend(bindings_in(body));
        let access = |stmts: &[Stmt]| {
            let mut acc = Access::default();
            for s in stmts {
                scan(
                    s.to_token_stream(),
                    &self.scope.mutable,
                    &self.scope.handles,
                    &mut acc,
                );
            }
            acc.rpaths.retain(|p| !is_local(&local, p));
            acc.wpaths.retain(|p| !is_local(&local, p));
            acc
        };
        let (a, b) = (access(&first), access(&rest));
        let b_all: Set = b.rpaths.union(&b.wpaths).cloned().collect();
        let independent = attrs.iter().any(|a| is_psync_attr(a, "independent"));
        if !independent
            && (conflict(&a.wpaths, &b_all).is_some() || conflict(&b.wpaths, &a.rpaths).is_some())
        {
            return None;
        }

        // Values the rest needs from each item: the pattern's bindings and the first part's.
        let mut defined: Vec<(Ident, bool)> = pattern.to_vec();
        for s in &first {
            if let Stmt::Local(l) = s {
                pat_idents(&l.pat, &mut defined);
            }
        }
        let needed: Set = rest
            .iter()
            .flat_map(|s| mentions(s.to_token_stream()))
            .collect();
        let mut carry: Vec<(Ident, bool)> = Vec::new();
        for (id, m) in defined
            .into_iter()
            .filter(|(id, _)| needed.contains(&id.to_string()))
        {
            match carry.iter_mut().find(|(c, _)| *c == id) {
                Some(slot) => *slot = (id, m),
                None => carry.push((id, m)),
            }
        }

        // First loop: exits that end the whole loop stop it and are performed afterwards.
        let k = self.next();
        let pre_label = loop_label
            .as_ref()
            .map(|l| l.name.clone())
            .unwrap_or_else(|| {
                syn::Lifetime::new(&format!("'__psync_pre{k}"), proc_macro2::Span::call_site())
            });
        let krate = self.options.krate.clone();
        let mut rewriter = ExitRewriter::deferring(&pre_label, format!("p{k}"), &krate);
        for s in &mut first {
            rewriter.visit_stmt_mut(s);
        }
        let slots = rewriter.slots;
        let slot_idents = slots.idents();
        let names: Vec<&Ident> = carry.iter().map(|(i, _)| i).collect();
        let pats = carry
            .iter()
            .map(|(i, m)| if *m { quote!(mut #i) } else { quote!(#i) });
        let items = format_ident!("__psync_first{}", k);

        // Second loop: over the first loop's results; prefetch it like any `for` loop.
        let attrs: Vec<&syn::Attribute> = attrs.iter().collect();
        let second: ExprForLoop = syn::parse_quote! {
            #(#attrs)* #loop_label for (#(#pats,)*) in #items { #(#rest)* }
        };
        let (second, prefetched) = self.prefetch(&second).ok()?;
        let reraise = slots.reraise(&krate);
        let tokens = quote! {{
            #(let mut #slot_idents = ::core::option::Option::None;)*
            let #items = {
                let mut __psync_v = ::std::vec::Vec::new();
                #pre_label: #head {
                    #(#first)*
                    __psync_v.push((#(#names,)*));
                }
                __psync_v
            };
            #second
            #reraise
        }};
        Some((
            syn::parse2(tokens).expect("psync::graph generated an invalid loop fission"),
            prefetched,
        ))
    }

    /// Split a loop body (or a guarded branch) into the statements that may run ahead of earlier
    /// iterations and the rest.
    ///
    /// Bindings and awaits are taken from the front of the body. Further on, an awaited statement
    /// may still join if nothing left behind before it could exit the iteration or touches what it
    /// touches. Awaits inside a scrutinee, call arguments or an `if` branch are hoisted out first.
    fn take_prefix(
        &mut self,
        body: Vec<Stmt>,
        allow_guard: bool,
        scope: &LoopScope,
    ) -> (Vec<Node>, Vec<Stmt>) {
        let mut prefix: Vec<Node> = Vec::new();
        let mut rest: Vec<Stmt> = Vec::new();
        let mut behind = Vec::<Node>::new();
        let mut blocked = false;
        let mut body = body.into_iter();
        while let Some(stmt) = body.next() {
            // `if c { ..; continue; }` then awaited work: run that work ahead when `!c`.
            if !blocked
                && allow_guard
                && let Some(c) = exit_guard(&stmt)
                && behind.is_empty()
            {
                let after: Vec<Stmt> = body.by_ref().collect();
                match self.invert_guard(c, after.clone(), scope) {
                    Some((guard, unpack)) if scope.refuses(&guard).is_none() => {
                        prefix.push(guard);
                        rest.push(stmt);
                        rest.extend(unpack);
                    }
                    _ => {
                        rest.push(stmt);
                        rest.extend(after);
                    }
                }
                break;
            }
            if !blocked
                && let Some((nodes, replacement)) = self.prefix_candidate(&stmt, allow_guard, scope)
                && nodes.iter().all(|n| scope.refuses(n).is_none())
                && nodes.iter().all(|n| {
                    behind
                        .iter()
                        .all(|b| crate::schedule::depends(b, n).is_none())
                })
                && (rest.is_empty() || nodes.iter().any(|n| n.awaits))
            {
                prefix.extend(nodes);
                rest.extend(replacement);
                continue;
            }
            match classify(&stmt, &self.scope, false) {
                Class::Node(n) => {
                    blocked |= n.exits;
                    behind.push(*n);
                }
                Class::Barrier(_) => blocked = true,
            }
            rest.push(stmt);
        }
        // Cheap leading bindings only belong in the prefix if an await there needs them.
        while prefix.last().is_some_and(|n| !n.awaits) {
            let n = prefix.pop().expect("non-empty");
            rest.insert(0, n.stmt);
        }
        (prefix, rest)
    }

    /// For an exit guard `if c { ..; continue; }` with pure `c`, take the awaited prefix of the
    /// statements after it, guarded by `!c`: returns the prefix node
    /// `let g = if !(c) { Some(<prefix chain>) } else { None };` and the statements that replace
    /// `after` (unpacking `g`, which is `Some` once the guard has not exited).
    fn invert_guard(
        &mut self,
        cond: &Expr,
        after: Vec<Stmt>,
        scope: &LoopScope,
    ) -> Option<(Node, Vec<Stmt>)> {
        let (inner, inner_rest) = self.take_prefix(after, false, scope);
        if inner.is_empty() {
            return None;
        }
        let needed: Set = inner_rest
            .iter()
            .flat_map(|s| mentions(s.to_token_stream()))
            .collect();
        let k = self.next();
        let krate = self.options.krate.clone();
        let members: Vec<&Node> = inner.iter().collect();
        let code = chain_code(&krate, &self.scope.fresh, &members, &needed, k, &[]);
        let (g, gout) = (
            format_ident!("__psync_g{}", k),
            format_ident!("__psync_gout{}", k),
        );
        let body = &code.body;
        let guard: Stmt = syn::parse_quote_spanned! {cond.span()=>
            let #g = if !(#cond) { ::core::option::Option::Some({ #body }) } else { ::core::option::Option::None };
        };
        let node = match classify(&guard, &self.scope, true) {
            Class::Node(n) if !n.effect && n.awaits => *n,
            _ => return None,
        };
        let slot_idents = code.slot_idents();
        let reraise = code.slots.iter().map(|s| s.reraise(&krate));
        let bindings = code.binding_pattern();
        let mut unpack: Vec<Stmt> = syn::parse_quote! {
            let ::core::option::Option::Some((#(#slot_idents,)* #gout)) = #g else {
                unreachable!("the guard did not exit, so its condition was false")
            };
            #(#reraise)*
            #[allow(unused_mut)] let #bindings = #gout.unwrap();
        };
        unpack.extend(inner_rest);
        Some((node, unpack))
    }

    /// The prefix nodes a statement contributes, and what stays in the body in its place.
    fn prefix_candidate(
        &mut self,
        stmt: &Stmt,
        allow_guard: bool,
        scope: &LoopScope,
    ) -> Option<(Vec<Node>, Option<Stmt>)> {
        let movable = |class: Class| match class {
            Class::Node(n) if !n.effect => Some(*n),
            _ => None,
        };
        if let Some(n) = movable(classify(stmt, &self.scope, true)) {
            return Some((vec![n], None));
        }
        let n = self.next();
        if let Some((value, rewritten)) = hoist::scrutinee(stmt, n) {
            let node = movable(classify(&value, &self.scope, true)).filter(|n| n.awaits)?;
            return Some((vec![node], Some(rewritten)));
        }
        if let Some((values, rewritten)) = hoist::operands(stmt, &mut self.fresh) {
            let nodes: Option<Vec<Node>> = values
                .iter()
                .map(|v| movable(classify(v, &self.scope, true)))
                .collect();
            return nodes
                .filter(|ns| ns.iter().any(|n| n.awaits))
                .map(|ns| (ns, Some(rewritten)));
        }
        if allow_guard && let Some((guard, rewritten)) = self.hoist_guard(stmt, scope) {
            let node = movable(classify(&guard, &self.scope, true)).filter(|n| n.awaits)?;
            return Some((vec![node], Some(rewritten)));
        }
        None
    }

    /// `if c { <awaited prefix>; rest } else { .. }` becomes a prefix binding
    /// `let g = if c { Some(<prefix chain>) } else { None };` and the statement
    /// `if let Some(..) = g { <re-raise exits>; rest } else { .. }`.
    fn hoist_guard(&mut self, stmt: &Stmt, scope: &LoopScope) -> Option<(Stmt, Stmt)> {
        let Stmt::Expr(Expr::If(i), semi) = stmt else {
            return None;
        };
        let cond = Exits::of(&i.cond);
        if cond.awaits || cond.exits || cond.blocker.is_some() {
            return None;
        }
        let (inner, inner_rest) = self.take_prefix(i.then_branch.stmts.clone(), false, scope);
        if inner.is_empty() {
            return None;
        }
        let mut bound_by_cond = Vec::new();
        if let Expr::Let(l) = &*i.cond {
            pat_idents(&l.pat, &mut bound_by_cond);
        }
        let needed: Set = inner_rest
            .iter()
            .flat_map(|s| mentions(s.to_token_stream()))
            .collect();
        let k = self.next();
        let krate = self.options.krate.clone();
        let members: Vec<&Node> = inner.iter().collect();
        let code = chain_code(
            &krate,
            &self.scope.fresh,
            &members,
            &needed,
            k,
            &bound_by_cond,
        );
        let (g, gout) = (
            format_ident!("__psync_g{}", k),
            format_ident!("__psync_gout{}", k),
        );
        let (cond, body) = (&i.cond, &code.body);
        let guard: Stmt = syn::parse_quote_spanned! {stmt.span()=>
            let #g = if #cond { ::core::option::Option::Some({ #body }) } else { ::core::option::Option::None };
        };
        let slot_idents = code.slot_idents();
        let reraise = code.slots.iter().map(|s| s.reraise(&krate));
        let bindings = code.binding_pattern();
        let else_branch = i.else_branch.as_ref().map(|(_, e)| quote!(else #e));
        let rewritten: Expr = syn::parse_quote! {
            if let ::core::option::Option::Some((#(#slot_idents,)* #gout)) = #g {
                #(#reraise)*
                #[allow(unused_mut)] let #bindings = #gout.unwrap();
                #(#inner_rest)*
            } #else_branch
        };
        Some((guard, Stmt::Expr(rewritten, *semi)))
    }
}
