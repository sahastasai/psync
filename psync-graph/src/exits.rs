//! Deferring early exits.
//!
//! When a node runs inside a concurrent chain, its `?`, `return`, `break` and `continue` cannot
//! leave the function or loop from inside the chain's future. [`ExitRewriter`] replaces each one
//! with "record the exit in a slot, then stop this chain" (`break '__psync_chainN None`), and
//! [`Slots::reraise`] performs the recorded exit after the join. Re-raising node by node in
//! program order makes the function exit exactly where sequential code would have, with the same
//! error value (the original `?` conversion happens at the re-raise).

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use syn::{Expr, visit_mut::VisitMut};

/// The exit slots of one node, declared as `let mut slot = None;` before its chain runs.
#[derive(Default)]
pub(crate) struct Slots {
    /// One per `?`: holds the residual (`Err` or `None`) if that `?` failed.
    tries: Vec<Ident>,
    /// Holds the value of a `return`.
    ret: Option<Ident>,
    /// Holds which `break`/`continue` was taken, as an index into `ctl_targets`.
    ctl: Option<Ident>,
    /// The statements that re-perform each recorded `break`/`continue`.
    ctl_targets: Vec<TokenStream>,
}

impl Slots {
    /// All slot variables, in declaration order.
    pub fn idents(&self) -> Vec<Ident> {
        self.tries
            .iter()
            .chain(&self.ret)
            .chain(&self.ctl)
            .cloned()
            .collect()
    }

    /// Statements that perform the recorded exit, if any, through `krate`'s runtime helpers.
    pub fn reraise(&self, krate: &TokenStream) -> TokenStream {
        let tries = &self.tries;
        let ret = self.ret.iter();
        let ctl = self.ctl.iter().map(|c| {
            let arms = self
                .ctl_targets
                .iter()
                .enumerate()
                .map(|(k, t)| quote!(::core::option::Option::Some(#k) => { #t; }));
            quote!(match #c { #(#arms)* _ => {} })
        });
        quote! {
            #(if let ::core::option::Option::Some(r) = #tries { #krate::__private::Rethrow::rethrow(r)?; })*
            #(if let ::core::option::Option::Some(v) = #ret { return v; })*
            #(#ctl)*
        }
    }
}

/// Rewrites the early exits of one node to record-and-stop. Closures, async blocks and nested
/// items keep their own exits; loops and labeled blocks inside the node keep exits aimed at them.
pub(crate) struct ExitRewriter<'a> {
    /// The chain's label, the target of every rewritten exit.
    chain: &'a syn::Lifetime,
    /// Unique prefix for this node's slot names.
    tag: String,
    krate: &'a TokenStream,
    pub slots: Slots,
    local_labels: Vec<String>,
    loop_depth: usize,
    /// Rewriting statements of a loop's first part (fission): `chain` labels that loop, which is
    /// stopped with a plain `break`, and exits aimed at it stay as written.
    deferring: bool,
}

impl<'a> ExitRewriter<'a> {
    /// Exits stop the chain labeled `chain` (`break 'chain None`).
    pub fn new(chain: &'a syn::Lifetime, tag: String, krate: &'a TokenStream) -> Self {
        Self {
            chain,
            tag,
            krate,
            slots: Slots::default(),
            local_labels: Vec::new(),
            loop_depth: 0,
            deferring: false,
        }
    }

    /// Exits that leave the loop labeled `chain` stop it and are recorded; `continue` and
    /// `break` aimed at that loop stay as written.
    pub fn deferring(chain: &'a syn::Lifetime, tag: String, krate: &'a TokenStream) -> Self {
        Self {
            deferring: true,
            ..Self::new(chain, tag, krate)
        }
    }

    /// True when a `break`/`continue` targets the loop being split, so it stays as written.
    fn targets_own_loop(&self, label: Option<&syn::Lifetime>) -> bool {
        self.deferring
            && label.is_none_or(|l| l.ident == self.chain.ident)
            && (label.is_some() || self.loop_depth == 0)
    }

    fn leaves(&self, label: Option<&syn::Lifetime>) -> bool {
        match label {
            Some(l) => {
                let name = l.ident.to_string();
                !name.starts_with("__psync") && !self.local_labels.contains(&name)
            }
            None => self.loop_depth == 0,
        }
    }

    fn nested(&mut self, label: Option<String>, f: impl FnOnce(&mut Self)) {
        let labeled = label.is_some();
        if let Some(l) = label {
            self.local_labels.push(l);
        }
        self.loop_depth += 1;
        f(self);
        self.loop_depth -= 1;
        if labeled {
            self.local_labels.pop();
        }
    }

    /// `{ slot = Some(value); break 'chain None }` (a plain `break 'chain` when deferring).
    fn record(&self, slot: &Ident, value: TokenStream) -> Expr {
        let chain = self.chain;
        let stop = if self.deferring {
            quote!(break #chain)
        } else {
            quote!(break #chain (::core::option::Option::None))
        };
        syn::parse_quote!({
            #slot = ::core::option::Option::Some(#value);
            #stop
        })
    }

    fn ctl(&mut self, target: TokenStream) -> Expr {
        let slot = self
            .slots
            .ctl
            .get_or_insert_with(|| format_ident!("__psync_c{}", self.tag))
            .clone();
        let index = self.slots.ctl_targets.len();
        self.slots.ctl_targets.push(target);
        self.record(&slot, quote!(#index))
    }
}

impl VisitMut for ExitRewriter<'_> {
    fn visit_expr_closure_mut(&mut self, _: &mut syn::ExprClosure) {}
    fn visit_expr_async_mut(&mut self, _: &mut syn::ExprAsync) {}
    fn visit_item_mut(&mut self, _: &mut syn::Item) {}

    fn visit_expr_mut(&mut self, e: &mut Expr) {
        match e {
            Expr::ForLoop(f) => {
                self.visit_expr_mut(&mut f.expr);
                let label = f.label.as_ref().map(|l| l.name.ident.to_string());
                self.nested(label, |s| s.visit_block_mut(&mut f.body));
                return;
            }
            Expr::While(w) => {
                self.visit_expr_mut(&mut w.cond);
                let label = w.label.as_ref().map(|l| l.name.ident.to_string());
                self.nested(label, |s| s.visit_block_mut(&mut w.body));
                return;
            }
            Expr::Loop(l) => {
                let label = l.label.as_ref().map(|l| l.name.ident.to_string());
                self.nested(label, |s| s.visit_block_mut(&mut l.body));
                return;
            }
            Expr::Block(b) if b.label.is_some() => {
                let label = b.label.as_ref().map(|l| l.name.ident.to_string());
                // A labeled block is not a loop: unlabeled `break`s inside still leave it.
                if let Some(l) = label {
                    self.local_labels.push(l);
                    self.visit_block_mut(&mut b.block);
                    self.local_labels.pop();
                }
                return;
            }
            _ => {}
        }
        // Rewrite inner exits first, so `a.await?.b()?` records both.
        syn::visit_mut::visit_expr_mut(self, e);
        let krate = self.krate;
        let replacement: Option<Expr> = match e {
            Expr::Try(t) => {
                let slot = format_ident!("__psync_e{}_{}", self.tag, self.slots.tries.len());
                self.slots.tries.push(slot.clone());
                let inner = &t.expr;
                let stop = self.record(&slot, quote!(r));
                Some(syn::parse_quote! {
                    match #krate::__private::Branch::branch(#inner) {
                        ::core::result::Result::Ok(v) => v,
                        ::core::result::Result::Err(r) => #stop,
                    }
                })
            }
            Expr::Return(r) => {
                let slot = self
                    .slots
                    .ret
                    .get_or_insert_with(|| format_ident!("__psync_r{}", self.tag))
                    .clone();
                let value = r.expr.as_ref().map(|v| quote!(#v)).unwrap_or(quote!(()));
                Some(self.record(&slot, value))
            }
            Expr::Continue(c)
                if self.leaves(c.label.as_ref()) && !self.targets_own_loop(c.label.as_ref()) =>
            {
                let label = &c.label;
                Some(self.ctl(quote!(continue #label)))
            }
            Expr::Break(b)
                if b.expr.is_none()
                    && self.leaves(b.label.as_ref())
                    && !self.targets_own_loop(b.label.as_ref()) =>
            {
                let label = &b.label;
                Some(self.ctl(quote!(break #label)))
            }
            _ => None,
        };
        if let Some(r) = replacement {
            *e = r;
        }
    }
}
