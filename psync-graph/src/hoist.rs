//! Exposing awaits that sit inside larger statements.
//!
//! An await in a `match` scrutinee, an `if` condition, or a call argument is evaluated before the
//! rest of its statement anyway. Hoisting it into its own binding (`let __psync_s1 = x.await;`)
//! keeps the evaluation order but turns it into a node that can overlap with other work, while
//! the remainder of the statement stays where it was.

use crate::classify::{Exits, is_place, is_pure};
use proc_macro2::Ident;
use quote::{ToTokens, format_ident};
use syn::{Expr, Local, LocalInit, Stmt, parse::Parser, punctuated::Punctuated, spanned::Spanned};

fn awaits(e: &Expr) -> bool {
    Exits::of(e).awaits
}

/// Split `match e.await {..}`, `if let p = e.await {..}`, `if e.await {..}`, the same forms as a
/// `let` initializer, and `let p = e.await else {..}` into `let __psync_sN = e.await;` and the
/// statement reading `__psync_sN`.
pub(crate) fn scrutinee(stmt: &Stmt, n: usize) -> Option<(Stmt, Stmt)> {
    let name = format_ident!("__psync_s{}", n);
    let take = |e: &mut Expr| -> Option<Expr> {
        let slot = match e {
            Expr::Match(m) => &mut m.expr,
            Expr::If(i) => match &mut *i.cond {
                Expr::Let(l) => &mut l.expr,
                _ => &mut i.cond,
            },
            _ => return None,
        };
        awaits(slot).then(|| std::mem::replace(&mut **slot, syn::parse_quote!(#name)))
    };
    let mut rewritten = stmt.clone();
    let value = match &mut rewritten {
        Stmt::Expr(e, _) => take(e)?,
        Stmt::Local(Local {
            init:
                Some(LocalInit {
                    expr,
                    diverge: None,
                    ..
                }),
            ..
        }) => take(expr)?,
        // `let p = e.await else { .. };`: the initializer is the scrutinee.
        Stmt::Local(Local {
            init:
                Some(LocalInit {
                    expr,
                    diverge: Some(_),
                    ..
                }),
            ..
        }) if awaits(expr) => std::mem::replace(&mut **expr, syn::parse_quote!(#name)),
        _ => return None,
    };
    Some((
        syn::parse_quote_spanned!(stmt.span()=> let #name = #value;),
        rewritten,
    ))
}

/// For loop fission: split `if c {..}` / `if let p = c {..}` / `match c {..}` whose branches
/// await but whose condition `c` does not into `let __psync_cN = c;` and the statement reading it.
pub(crate) fn condition(stmt: &Stmt, n: usize) -> Option<(Stmt, Stmt)> {
    let name = format_ident!("__psync_c{}", n);
    let Stmt::Expr(e, semi) = stmt else {
        return None;
    };
    let mut e = e.clone();
    let slot = match &mut e {
        Expr::Match(m) => &mut m.expr,
        Expr::If(i) => match &mut *i.cond {
            Expr::Let(l) => &mut l.expr,
            _ => &mut i.cond,
        },
        _ => return None,
    };
    if awaits(slot) || is_pure(slot) {
        return None;
    }
    let value = std::mem::replace(&mut **slot, syn::parse_quote!(#name));
    Some((
        syn::parse_quote_spanned!(stmt.span()=> let #name = #value;),
        Stmt::Expr(e, *semi),
    ))
}

fn is_compound_assign(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        AddAssign(_)
            | SubAssign(_)
            | MulAssign(_)
            | DivAssign(_)
            | RemAssign(_)
            | BitXorAssign(_)
            | BitAndAssign(_)
            | BitOrAssign(_)
            | ShlAssign(_)
            | ShrAssign(_)
    )
}

/// Hoist the awaited operands of a call, method call, assignment, array, tuple or `vec!` into
/// bindings, together with any earlier operands that are not pure, so they still evaluate in
/// order. Also hoists an awaited method receiver (`x.await?.items()`), which evaluates first.
/// `fresh` numbers the new bindings.
pub(crate) fn operands(stmt: &Stmt, fresh: &mut usize) -> Option<(Vec<Stmt>, Stmt)> {
    let mut rewritten = stmt.clone();
    let mut target: &mut Expr = match &mut rewritten {
        Stmt::Expr(e, _) => e,
        Stmt::Local(Local {
            init:
                Some(LocalInit {
                    expr,
                    diverge: None,
                    ..
                }),
            ..
        }) => expr,
        _ => return None,
    };
    if let Expr::Try(t) = target {
        target = &mut t.expr;
    }
    let mut vec_args: Option<Punctuated<Expr, syn::Token![,]>> = None;
    let operands: Vec<&mut Expr> = match target {
        Expr::MethodCall(m) if !is_place(&m.receiver) && awaits(&m.receiver) => {
            vec![&mut *m.receiver]
        }
        Expr::MethodCall(m) if is_place(&m.receiver) => m.args.iter_mut().collect(),
        Expr::Call(c) if matches!(&*c.func, Expr::Path(_)) => c.args.iter_mut().collect(),
        Expr::Assign(a) if is_place(&a.left) => vec![&mut *a.right],
        // `x += y`: with a side-effect-free place on the left, `y` may be evaluated first.
        Expr::Binary(b) if is_compound_assign(&b.op) && is_place(&b.left) => vec![&mut *b.right],
        Expr::Array(a) => a.elems.iter_mut().collect(),
        Expr::Tuple(t) => t.elems.iter_mut().collect(),
        Expr::Macro(m) if m.mac.path.is_ident("vec") => {
            vec_args = Punctuated::parse_terminated
                .parse2(m.mac.tokens.clone())
                .ok();
            vec_args.as_mut()?.iter_mut().collect()
        }
        _ => return None,
    };
    let last = operands.iter().rposition(|e| awaits(e))?;
    let mut lets = Vec::new();
    for e in operands.into_iter().take(last + 1) {
        if is_pure(e) && !awaits(e) {
            continue;
        }
        *fresh += 1;
        let h: Ident = format_ident!("__psync_h{}", *fresh);
        let value = std::mem::replace(e, syn::parse_quote!(#h));
        lets.push(syn::parse_quote_spanned!(stmt.span()=> let #h = #value;));
    }
    if lets.is_empty() {
        return None;
    }
    if let (Some(args), Expr::Macro(m)) = (vec_args, &mut *target) {
        m.mac.tokens = args.to_token_stream();
    }
    Some((lets, rewritten))
}
