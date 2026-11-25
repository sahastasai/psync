//! Runtime checks of computed file paths in straight-line code.
//!
//! `fs::write(&a, ..)` and `fs::read(&b)` conflict when the macro cannot tell `a` from `b`. When
//! both paths can be evaluated at the start of the region (they are total and depend on nothing
//! the region binds or anything `mut`), the region is scheduled as if differently written paths
//! named different files, and each dependency that drops is kept at run time instead: the paths
//! are compared at the start of the region, and when they overlap, the later chain waits for the
//! earlier one to end before it starts ([`crate::__private`]'s `Gate`).

use crate::access::{Set, mentions};
use crate::classify::Node;
use crate::effects::{Domain, Target, conflict};
use crate::loops::{substitute, total};
use crate::schedule::{Plan, depends, plan};
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use std::collections::HashMap;
use syn::{Expr, Stmt};

/// A gate: chain `waits` starts only after chain `on` ends, when `cond` (a `bool` computed at the
/// start of the region) is true.
pub(crate) struct Gate {
    pub on: usize,
    pub waits: usize,
    pub cond: proc_macro2::Ident,
}

/// The relaxed schedule, the gates that keep it sequentially consistent, and the statements that
/// compute their conditions; `None` when there is nothing to gain or a gate cannot be placed.
pub(crate) fn relax(
    nodes: &[Node],
    mutable: &Set,
    ids: &mut usize,
) -> Option<(Plan, Vec<Gate>, Vec<Stmt>)> {
    // Every name the region binds, with repetitions.
    let mut bound: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        for (i, _) in &n.defs {
            *bound.entry(i.to_string()).or_default() += 1;
        }
    }
    // `let p = dir.join("a");` in the region: `p` stands for its value.
    let mut lets: HashMap<String, TokenStream> = HashMap::new();
    for n in nodes {
        let Stmt::Local(l) = &n.stmt else { continue };
        let p = match &l.pat {
            syn::Pat::Type(t) => &*t.pat,
            p => p,
        };
        if let (syn::Pat::Ident(id), Some(init)) = (p, &l.init)
            && id.by_ref.is_none()
            && id.mutability.is_none()
            && id.subpat.is_none()
            && init.diverge.is_none()
            && bound.get(&id.ident.to_string()) == Some(&1)
        {
            let value = substitute(init.expr.to_token_stream(), &lets);
            if syn::parse2::<Expr>(value.clone()).is_ok_and(|e| total(&e)) {
                lets.insert(id.ident.to_string(), value);
            }
        }
    }
    // The key of each file effect whose path can be evaluated at the start of the region.
    let key = |source: &Option<String>| -> Option<Expr> {
        let tokens: TokenStream = source.as_ref()?.parse().ok()?;
        let expr: Expr = syn::parse2(substitute(tokens, &lets)).ok()?;
        let used = mentions(expr.to_token_stream());
        (total(&expr)
            && !used
                .iter()
                .any(|n| bound.contains_key(n) || mutable.contains(n)))
        .then_some(expr)
    };
    let keys: Vec<Vec<Option<Expr>>> = nodes
        .iter()
        .map(|n| {
            n.effects
                .iter()
                .map(|e| (e.domain == Domain::Fs).then(|| key(&e.source)).flatten())
                .collect()
        })
        .collect();
    if keys.iter().flatten().filter(|k| k.is_some()).count() < 2 {
        return None;
    }
    // Differently written keys stand for different files; the same key, for the same file.
    let mut relaxed: Vec<Node> = nodes.to_vec();
    for (n, ks) in relaxed.iter_mut().zip(&keys) {
        for (e, k) in n.effects.iter_mut().zip(ks) {
            if let Some(k) = k {
                e.target =
                    Target::Literal(format!("`{}` (compared at run time)", k.to_token_stream()));
            }
        }
    }
    // Each dependency that dropped must have been only through keyed effect pairs.
    type Dropped = (usize, usize, Vec<(Expr, Expr)>);
    let mut dropped: Vec<Dropped> = Vec::new();
    for j in 0..nodes.len() {
        for i in 0..j {
            if depends(&nodes[i], &nodes[j]).is_none()
                || depends(&relaxed[i], &relaxed[j]).is_some()
            {
                continue;
            }
            let mut pairs = Vec::new();
            for (a, ka) in nodes[i].effects.iter().zip(&keys[i]) {
                for (b, kb) in nodes[j].effects.iter().zip(&keys[j]) {
                    if conflict(a, b).is_none() {
                        continue;
                    }
                    pairs.push((ka.clone()?, kb.clone()?));
                }
            }
            dropped.push((i, j, pairs));
        }
    }
    if dropped.is_empty() {
        return None;
    }
    let relaxed_plan = plan(&relaxed);
    let strict = plan(nodes);
    if relaxed_plan.levels.len() >= strict.levels.len()
        && relaxed_plan.chains.len() <= strict.chains.len()
    {
        return None;
    }
    let mut chain_of = vec![0; nodes.len()];
    for (c, members) in relaxed_plan.chains.iter().enumerate() {
        for &v in members {
            chain_of[v] = c;
        }
    }
    let mut level_of = vec![0; relaxed_plan.chains.len()];
    for (l, chains) in relaxed_plan.levels.iter().enumerate() {
        for &c in chains {
            level_of[c] = l;
        }
    }
    // A chain waits for one of its own level: an earlier level has ended before it starts, and a
    // later one would never start.
    let mut conds: HashMap<(usize, usize), Vec<(Expr, Expr)>> = HashMap::new();
    for (i, j, pairs) in dropped {
        let (x, y) = (chain_of[i], chain_of[j]);
        if x == y || level_of[x] < level_of[y] {
            continue;
        }
        // A gate runs from the chain that starts first, so gates cannot wait in a circle.
        if level_of[x] > level_of[y] || relaxed_plan.chains[x][0] > relaxed_plan.chains[y][0] {
            return None;
        }
        conds.entry((x, y)).or_default().extend(pairs);
    }
    let mut gates = Vec::new();
    let mut prelude: Vec<Stmt> = Vec::new();
    let mut conds: Vec<_> = conds.into_iter().collect();
    conds.sort_by_key(|(k, _)| *k);
    for ((on, waits), pairs) in conds {
        *ids += 1;
        let cond = format_ident!("__psync_overlap{}", *ids);
        let tests = pairs
            .iter()
            .map(|(a, b)| quote!(__psync_overlap(&(#a), &(#b))));
        prelude.push(syn::parse_quote!(let #cond: bool = false #(|| #tests)*;));
        gates.push(Gate { on, waits, cond });
    }
    if gates.is_empty() {
        return Some((relaxed_plan, gates, prelude));
    }
    // Paths overlap when one is the other or a directory that contains it, after lexical
    // normalization.
    let helper: Stmt = syn::parse_quote! {
        #[allow(unused)]
        let __psync_overlap = |a: &dyn ::core::convert::AsRef<::std::path::Path>,
                               b: &dyn ::core::convert::AsRef<::std::path::Path>| {
            let norm = |p: &dyn ::core::convert::AsRef<::std::path::Path>| -> ::std::path::PathBuf {
                p.as_ref()
                    .components()
                    .filter(|c| !::core::matches!(c, ::std::path::Component::CurDir))
                    .collect()
            };
            let (a, b) = (norm(a), norm(b));
            a.starts_with(&b) || b.starts_with(&a)
        };
    };
    prelude.insert(0, helper);
    Some((relaxed_plan, gates, prelude))
}
