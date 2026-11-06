//! Building the dependency graph of a region and generating its concurrent schedule.
//!
//! A *region* is a run of nodes between barriers. Its graph has an edge `u -> v` whenever `v`
//! must wait for `u` ([`depends`]). Nodes with a single predecessor whose predecessor has a single
//! successor are fused into *chains*, so per-item pipelines (`a1 -> b1`, `a2 -> b2`) proceed
//! independently. Chains are then leveled; each level with two or more chains becomes one join of
//! their futures, and the results are bound in program order.

use crate::RegionReport;
use crate::access::{Set, conflict};
use crate::classify::{HINTS, Node, is_psync_attr};
use crate::exits::{ExitRewriter, Slots};
use proc_macro2::{Ident, Span, TokenStream};
use quote::{ToTokens, format_ident, quote};
use std::collections::HashMap;
use syn::{Stmt, visit_mut::VisitMut};

/// Why `b` (later in the program) must wait for `a` (earlier), if it must.
pub(crate) fn depends(a: &Node, b: &Node) -> Option<String> {
    let names = |n: &Node| -> Set { n.defs.iter().map(|(i, _)| i.to_string()).collect() };
    let (a_defs, b_defs) = (names(a), names(b));
    // Data flow: `b` reads a binding `a` creates.
    if let Some(x) = a_defs.intersection(&b.acc.uses).next() {
        return Some(format!("reads `{x}`"));
    }
    // Shadowing: both bind the name, or `b` rebinds a name `a` still reads.
    if let Some(x) = a_defs.intersection(&b_defs).next() {
        return Some(format!("rebinds `{x}`"));
    }
    if let Some(x) = a.acc.uses.intersection(&b_defs).next() {
        return Some(format!("shadows `{x}`"));
    }
    // Conflicting accesses to the same place.
    let b_all: Set = b.acc.rpaths.union(&b.acc.wpaths).cloned().collect();
    if let Some(x) =
        conflict(&a.acc.wpaths, &b_all).or_else(|| conflict(&b.acc.wpaths, &a.acc.rpaths))
    {
        return Some(format!("writes `{x}`"));
    }
    // Ordinary statements keep their order relative to each other. A binding or await may still
    // run ahead of one that might leave the block: that is the speculation the attribute allows,
    // and the exit is still performed in program order.
    if a.effect && b.effect {
        return Some("effect order".into());
    }
    None
}

/// The schedule of one region.
pub(crate) struct Plan {
    /// Fused chains: node indices in program order.
    pub chains: Vec<Vec<usize>>,
    /// Chain indices that run concurrently, level by level.
    pub levels: Vec<Vec<usize>>,
    pub report: RegionReport,
}

/// Build the graph, fuse chains and level them.
pub(crate) fn plan(nodes: &[Node]) -> Plan {
    let n = nodes.len();
    let mut preds = vec![Vec::new(); n];
    let mut succs = vec![Vec::new(); n];
    let mut edges = Vec::new();
    for j in 0..n {
        for i in 0..j {
            if let Some(why) = depends(&nodes[i], &nodes[j]) {
                preds[j].push(i);
                succs[i].push(j);
                edges.push((i, j, why));
            }
        }
    }
    // Fuse `u -> v` when that edge is the only way into `v` and either the only way out of `u`,
    // or `u` does not wait and everything else after `u` waits for `v` anyway (`let key = ..;`
    // read by a request and by what uses its response): fusing then delays nothing.
    let mut chain_of = vec![usize::MAX; n];
    let mut chains: Vec<Vec<usize>> = Vec::new();
    for v in 0..n {
        if let [u] = preds[v][..]
            && chains[chain_of[u]].last() == Some(&u)
            && (succs[u].len() == 1
                || !nodes[u].awaits && {
                    let after_v = reachable(&succs, v);
                    succs[u].iter().all(|w| *w == v || after_v[*w])
                })
        {
            let c = chain_of[u];
            chains[c].push(v);
            chain_of[v] = c;
        } else {
            chain_of[v] = chains.len();
            chains.push(vec![v]);
        }
    }
    // A chain's level is one past the deepest chain it waits for.
    let mut level = vec![0usize; chains.len()];
    for (c, members) in chains.iter().enumerate() {
        for &v in members {
            for &p in &preds[v] {
                if chain_of[p] != c {
                    level[c] = level[c].max(level[chain_of[p]] + 1);
                }
            }
        }
    }
    let depth = level.iter().max().map_or(0, |m| m + 1);
    let levels: Vec<Vec<usize>> = (0..depth)
        .map(|l| (0..chains.len()).filter(|c| level[*c] == l).collect())
        .collect();
    let report = RegionReport {
        nodes: nodes.iter().map(crate::NodeReport::from).collect(),
        edges,
        chains: chains.clone(),
        levels: levels.clone(),
        changed: levels.iter().any(|l| l.len() > 1),
    };
    Plan {
        chains,
        levels,
        report,
    }
}

/// The nodes that (transitively) depend on `from`; nodes are in program order, so edges go forward.
fn reachable(succs: &[Vec<usize>], from: usize) -> Vec<bool> {
    let mut seen = vec![false; succs.len()];
    for v in from..succs.len() {
        if v == from || seen[v] {
            for &w in &succs[v] {
                seen[w] = true;
            }
        }
    }
    seen
}

/// Generated code for one chain.
pub(crate) struct ChainCode {
    /// The chain's body: evaluates to `(slots..., Option<(bindings...)>)`.
    pub body: TokenStream,
    /// Exit slots per member, in program order.
    pub slots: Vec<Slots>,
    /// Bindings handed back to the enclosing scope, with their `mut` flag.
    pub bindings: Vec<(Ident, bool)>,
}

impl ChainCode {
    /// All slot variables, matching the order of the body's result tuple.
    pub fn slot_idents(&self) -> Vec<Ident> {
        self.slots.iter().flat_map(Slots::idents).collect()
    }
    /// `(mut a, b,)`: the pattern that rebinds the handed-back values.
    pub fn binding_pattern(&self) -> TokenStream {
        let pats = self
            .bindings
            .iter()
            .map(|(id, m)| if *m { quote!(mut #id) } else { quote!(#id) });
        quote!((#(#pats,)*))
    }
}

/// Generate a chain body: the members' statements in order, with early exits recorded instead of
/// taken. `needed` lists names used after the chain; `extra` adds bindings defined outside the
/// members (a loop's pattern) that should be handed back too.
pub(crate) fn chain_code(
    krate: &TokenStream,
    fresh: &std::collections::BTreeMap<String, (Ident, TokenStream)>,
    members: &[&Node],
    needed: &Set,
    k: usize,
    extra: &[(Ident, bool)],
) -> ChainCode {
    let label = syn::Lifetime::new(&format!("'__psync_chain{k}"), Span::call_site());
    // Bindings are only mutated after the chain hands them back, so `mut` is dropped inside the
    // chain unless the chain itself writes them (the handed-back binding keeps it).
    let written: Set = members
        .iter()
        .flat_map(|n| n.acc.writes.iter().cloned())
        .collect();
    let mut stmts = Vec::new();
    let mut slots = Vec::new();
    for (m, node) in members.iter().enumerate() {
        let mut stmt = node.stmt.clone();
        StripHints.visit_stmt_mut(&mut stmt);
        if let Stmt::Local(l) = &mut stmt {
            DropMut(&written).visit_pat_mut(&mut l.pat);
        }
        let mut rewriter = ExitRewriter::new(&label, format!("{k}_{m}"), krate);
        rewriter.visit_stmt_mut(&mut stmt);
        stmts.push(stmt.into_token_stream());
        slots.push(rewriter.slots);
    }
    // The last binding of each name wins, as shadowing does in the original.
    let mut bindings: Vec<(Ident, bool)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (id, m) in extra
        .iter()
        .cloned()
        .chain(members.iter().flat_map(|n| n.defs.iter().cloned()))
    {
        let name = id.to_string();
        if !needed.contains(&name) {
            continue;
        }
        match seen.get(&name) {
            Some(&at) => bindings[at] = (id, m),
            None => {
                seen.insert(name, bindings.len());
                bindings.push((id, m));
            }
        }
    }
    let names: Vec<&Ident> = bindings.iter().map(|(i, _)| i).collect();
    let slot_idents: Vec<Ident> = slots.iter().flat_map(Slots::idents).collect();
    // Each chain makes its own copy of the `#[psync::fresh]` bindings it uses.
    // The binding reuses the original identifier so macro hygiene resolves uses to it.
    let fresh_lets = fresh
        .iter()
        .filter(|(name, _)| members.iter().any(|n| n.acc.uses.contains(*name)))
        .map(|(_, (id, init))| quote!(let #id = #init;));
    let body = quote! {
        #(#fresh_lets)*
        #(let mut #slot_idents = ::core::option::Option::None;)*
        #[allow(unused_labels, unreachable_code, clippy::let_unit_value)]
        let __psync_out = #label: {
            #(#stmts)*
            ::core::option::Option::Some((#(#names,)*))
        };
        (#(#slot_idents,)* __psync_out)
    };
    ChainCode {
        body,
        slots,
        bindings,
    }
}

/// Generate a region's statements from its plan. `positions` are the nodes' statement indices
/// in the block and `mentions` what each statement of the block mentions.
pub(crate) fn emit(
    krate: &TokenStream,
    fresh: &std::collections::BTreeMap<String, (Ident, TokenStream)>,
    nodes: Vec<Node>,
    plan: &Plan,
    positions: &[usize],
    mentions: &[Set],
) -> Vec<Stmt> {
    let mut out = Vec::new();
    for level in &plan.levels {
        // A level with one chain runs exactly as written.
        if let [only] = level[..] {
            out.extend(plan.chains[only].iter().map(|&v| nodes[v].stmt.clone()));
            continue;
        }
        let mut futures = Vec::new();
        let mut takes = Vec::new();
        let mut binds = Vec::new();
        let mut reraises: Vec<(usize, TokenStream)> = Vec::new();
        for (k, &c) in level.iter().enumerate() {
            let members: Vec<&Node> = plan.chains[c].iter().map(|&v| &nodes[v]).collect();
            let chain_positions: Vec<usize> =
                plan.chains[c].iter().map(|&v| positions[v]).collect();
            // Bindings needed later: mentioned by any later statement outside this chain.
            let needed: Set = mentions
                .iter()
                .enumerate()
                .filter(|(i, _)| *i > chain_positions[0] && !chain_positions.contains(i))
                .flat_map(|(_, m)| m.iter().cloned())
                .collect();
            let code = chain_code(krate, fresh, &members, &needed, k, &[]);
            let (f, out_k) = (
                format_ident!("__psync_f{}", k),
                format_ident!("__psync_out{}", k),
            );
            let body = &code.body;
            futures.push(quote!(let mut #f = #krate::__private::maybe_done(async { #body });));
            let slot_idents = code.slot_idents();
            let taken = format_ident!("__psync_taken{}", k);
            takes.push(quote!(let (#(#slot_idents,)* #out_k) = #taken;));
            for (slots, pos) in code.slots.iter().zip(chain_positions) {
                reraises.push((pos, slots.reraise(krate)));
            }
            let pattern = code.binding_pattern();
            binds.push(quote!(#[allow(unused_mut)] let #pattern = #out_k.unwrap();));
        }
        // Exits are performed in program order, so the first one sequential code would hit wins.
        reraises.sort_by_key(|(p, _)| *p);
        let reraises = reraises.into_iter().map(|(_, r)| r);
        let fs = (0..level.len()).map(|k| format_ident!("__psync_f{}", k));
        let done: Vec<Ident> = (0..level.len())
            .map(|k| format_ident!("__psync_done{}", k))
            .collect();
        let fs: Vec<Ident> = fs.collect();
        let taken = (0..level.len()).map(|k| format_ident!("__psync_taken{}", k));
        // The futures live in an inner block: they (and whatever they borrow) are dropped as soon
        // as their outputs are taken, so later statements may move what the branches borrowed.
        let tokens = quote! {{
            let (#(#taken,)*) = {
                #(#futures)*
                ::core::future::poll_fn(|__psync_cx| {
                    #(let #done = #fs.poll_step(__psync_cx);)*
                    if true #(&& #done)* { ::core::task::Poll::Ready(()) } else { ::core::task::Poll::Pending }
                }).await;
                (#(#fs.take(),)*)
            };
            #(#takes)*
            #(#reraises)*
            #(#binds)*
        }};
        let block: syn::Block =
            syn::parse2(tokens).expect("psync::graph generated an invalid level");
        out.extend(block.stmts);
    }
    out
}

/// Drops `mut` from bindings the chain does not write.
pub(crate) struct DropMut<'a>(pub &'a Set);
impl VisitMut for DropMut<'_> {
    fn visit_pat_ident_mut(&mut self, p: &mut syn::PatIdent) {
        if !self.0.contains(&p.ident.to_string()) {
            p.mutability = None;
        }
        syn::visit_mut::visit_pat_ident_mut(self, p);
    }
}

/// Removes psync's hint attributes (`seq`, `fresh`, `independent`), which only guide the analysis.
pub(crate) struct StripHints;
impl VisitMut for StripHints {
    fn visit_attributes_mut(&mut self, attrs: &mut Vec<syn::Attribute>) {
        // Branches use their own copy of a `fresh` binding, so the original may go unused.
        for a in attrs.iter_mut().filter(|a| is_psync_attr(a, "fresh")) {
            *a = syn::parse_quote!(#[allow(unused_variables)]);
        }
        attrs.retain(|a| !HINTS.iter().any(|h| is_psync_attr(a, h)));
    }
    fn visit_expr_for_loop_mut(&mut self, f: &mut syn::ExprForLoop) {
        self.visit_attributes_mut(&mut f.attrs);
        syn::visit_mut::visit_expr_for_loop_mut(self, f);
    }
    fn visit_local_mut(&mut self, l: &mut syn::Local) {
        self.visit_attributes_mut(&mut l.attrs);
        syn::visit_mut::visit_local_mut(self, l);
    }
    fn visit_stmt_macro_mut(&mut self, m: &mut syn::StmtMacro) {
        self.visit_attributes_mut(&mut m.attrs);
    }
}
