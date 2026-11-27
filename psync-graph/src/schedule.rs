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
    // Effects on the same file, service or other outside resource keep their order.
    if !(a.independent || b.independent)
        && let Some(why) = crate::effects::conflicts(&a.effects, &b.effects)
    {
        return Some(why);
    }
    // A write is not performed before an earlier statement that may leave: sequential code
    // would not have performed it at all.
    if (a.exits || a.panics)
        && !b.speculate
        && let Some(w) = b.effects.iter().find(|e| e.real_write())
    {
        // The call's name; where the effect came from is said with the effect itself.
        let op = w.op.strip_suffix(", per the compiler").unwrap_or(&w.op);
        return Some(format!("may exit before `{op}` runs"));
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
    /// Per chain, the level after which its exits are replayed and its bindings handed back: its
    /// own level, or later when a statement before one of its exits runs in a later level.
    pub release: Vec<usize>,
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
    // Fuse `u -> v` when every edge into `v` comes from the chain that ends in `u` (members of a
    // chain run in order, so `u` running means they all have), and either that is the only way
    // out of `u`, or `u` does not wait and everything else after `u` waits for `v` anyway
    // (`let key = ..;` read by a request and by what uses its response): fusing then delays
    // nothing.
    let mut chain_of = vec![usize::MAX; n];
    let mut chains: Vec<Vec<usize>> = Vec::new();
    for v in 0..n {
        if let Some(&u) = preds[v].iter().max()
            && chains[chain_of[u]].last() == Some(&u)
            && preds[v].iter().all(|p| chain_of[*p] == chain_of[u])
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
    // A chain's level is one past the release of the deepest chain it waits for. A chain's exits
    // are replayed only once every statement before them in program order has run, as sequential
    // code would run those first (and an exit of theirs would win). Until then the chain's
    // bindings are not handed back, so what waits for it starts after its release. Chains are in
    // topological order (only a chain's first member has edges from other chains), and levels
    // only grow, so this settles.
    let mut level = vec![0usize; chains.len()];
    let mut release = vec![0usize; chains.len()];
    loop {
        let mut changed = false;
        for (c, members) in chains.iter().enumerate() {
            let mut l = level[c];
            for &v in members {
                for &p in &preds[v] {
                    if chain_of[p] != c {
                        l = l.max(release[chain_of[p]] + 1);
                    }
                }
            }
            changed |= l != level[c];
            level[c] = l;
        }
        for (c, members) in chains.iter().enumerate() {
            let mut r = level[c];
            for &v in members.iter().filter(|&&v| nodes[v].exits) {
                for u in 0..v {
                    r = r.max(level[chain_of[u]]);
                }
            }
            changed |= r != release[c];
            release[c] = r;
        }
        if !changed {
            break;
        }
    }
    let depth = level.iter().max().map_or(0, |m| m + 1);
    let levels: Vec<Vec<usize>> = (0..depth)
        .map(|l| (0..chains.len()).filter(|c| level[*c] == l).collect())
        .collect();
    let report = RegionReport {
        nodes: nodes.iter().map(crate::NodeReport::from).collect(),
        unclassified: nodes
            .iter()
            .flat_map(|n| n.unclassified.iter().map(move |u| (n.line, u.clone())))
            .collect(),
        edges,
        chains: chains.clone(),
        levels: levels.clone(),
        changed: levels.iter().any(|l| l.len() > 1),
        cert: cert_nodes(nodes),
        runtime_checks: 0,
    };
    Plan {
        chains,
        levels,
        release,
        report,
    }
}

/// Each node's places, for the region's certificate: one place per reason `depends` can give,
/// so that two nodes have no edge only if neither writes a place the other touches (the checker
/// in `verify/Cert.lean` may be stricter than `depends`, never more lenient).
fn cert_nodes(nodes: &[Node]) -> Vec<crate::CertNode> {
    let one = |ns: &str, rest: &[&str]| -> Vec<String> {
        std::iter::once(ns)
            .chain(rest.iter().copied())
            .map(str::to_string)
            .collect()
    };
    let path = |p: &str| -> Vec<String> {
        std::iter::once("p")
            .chain(p.split('.'))
            .map(str::to_string)
            .collect()
    };
    nodes
        .iter()
        .enumerate()
        .map(|(j, n)| {
            let mut c = crate::CertNode::default();
            for (d, _) in &n.defs {
                c.writes.push(one("n", &[&d.to_string()]));
            }
            for u in &n.acc.uses {
                c.reads.push(one("n", &[u]));
            }
            for p in &n.acc.rpaths {
                c.reads.push(path(p));
            }
            for p in &n.acc.wpaths {
                c.writes.push(path(p));
            }
            for e in &n.effects {
                // `independent` statements commute on the outside world; hidden state is
                // listed either way.
                if n.independent && !matches!(e.domain, crate::effects::Domain::State(_)) {
                    continue;
                }
                if e.writes() {
                    c.writes.push(e.place());
                } else {
                    c.reads.push(e.place());
                }
                c.reads.extend(e.extra_reads());
                if e.observed() {
                    c.observed.push(e.place());
                }
            }
            if n.exits || n.panics {
                c.writes.push(one("x", &[&j.to_string()]));
            }
            if !n.speculate && n.effects.iter().any(|e| e.real_write()) {
                for (i, m) in nodes[..j].iter().enumerate() {
                    if m.exits || m.panics {
                        c.reads.push(one("x", &[&i.to_string()]));
                    }
                }
            }
            if n.effect {
                c.writes.push(one("e", &[]));
            }
            c
        })
        .collect()
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
    chain_code_with(krate, fresh, members, needed, k, extra, None)
}

/// [`chain_code`], optionally storing the index of the member about to run in the atomic
/// `progress` (synchronous code: a panic is replayed at the member that raised it).
pub(crate) fn chain_code_with(
    krate: &TokenStream,
    fresh: &std::collections::BTreeMap<String, (Ident, TokenStream)>,
    members: &[&Node],
    needed: &Set,
    k: usize,
    extra: &[(Ident, bool)],
    progress: Option<&Ident>,
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
        // A loop body may end in `api.put(x).await` with no `;` (its value is `()`). In a chain the
        // member is followed by more code, so it needs one.
        if let Stmt::Expr(e, semi @ None) = &mut stmt
            && !matches!(
                e,
                syn::Expr::If(_)
                    | syn::Expr::Match(_)
                    | syn::Expr::Block(_)
                    | syn::Expr::ForLoop(_)
                    | syn::Expr::While(_)
                    | syn::Expr::Loop(_)
                    | syn::Expr::Unsafe(_)
            )
        {
            *semi = Some(Default::default());
        }
        if let Some(p) = progress {
            let m = proc_macro2::Literal::usize_unsuffixed(m);
            stmts.push(quote!(#p.store(#m, ::core::sync::atomic::Ordering::Relaxed);));
        }
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit(
    sync: bool,
    krate: &TokenStream,
    fresh: &std::collections::BTreeMap<String, (Ident, TokenStream)>,
    nodes: Vec<Node>,
    plan: &Plan,
    positions: &[usize],
    mentions: &[Set],
    ids: &mut usize,
    gates: &[crate::gates::Gate],
) -> Vec<Stmt> {
    let mut out = Vec::new();
    // Exit replays and bindings deferred to a later level: (level, position, tokens).
    let mut late_reraises: Vec<(usize, usize, TokenStream)> = Vec::new();
    let mut late_binds: Vec<(usize, TokenStream)> = Vec::new();
    // Chains are numbered across the region, so a deferred chain's slots are not shadowed.
    let mut chain_no = 0usize;
    for (l, level) in plan.levels.iter().enumerate() {
        let mut reraises: Vec<(usize, TokenStream)> = Vec::new();
        let mut binds = Vec::new();
        for (at, pos, r) in std::mem::take(&mut late_reraises) {
            if at == l {
                reraises.push((pos, r))
            } else {
                late_reraises.push((at, pos, r))
            }
        }
        for (at, b) in std::mem::take(&mut late_binds) {
            if at == l {
                binds.push(b)
            } else {
                late_binds.push((at, b))
            }
        }
        // A level with one chain runs exactly as written.
        if let [only] = level[..]
            && plan.release[only] == l
        {
            out.extend(plan.chains[only].iter().map(|&v| nodes[v].stmt.clone()));
            reraises.sort_by_key(|(p, _)| *p);
            let (r, b) = (reraises.into_iter().map(|(_, r)| r), binds);
            let block: syn::Block = syn::parse2(quote!({ #(#r)* #(#b)* }))
                .expect("psync::graph generated an invalid level");
            out.extend(block.stmts);
            continue;
        }
        let mut futures = Vec::new();
        let mut takes = Vec::new();
        *ids += 1;
        let level_id = proc_macro2::Literal::usize_unsuffixed(*ids);
        let gate_of = |c: usize| format_ident!("__psync_gate{}_{}", *ids, c);
        for &c in level {
            if gates.iter().any(|g| g.on == c) {
                let g = gate_of(c);
                futures.push(quote!(let #g = #krate::__private::Gate::new();));
            }
        }
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
            let progress = format_ident!("__psync_at{}", chain_no);
            let code = chain_code_with(
                krate,
                fresh,
                &members,
                &needed,
                chain_no,
                &[],
                sync.then_some(&progress),
            );
            let (f, out_k) = (
                format_ident!("__psync_f{}", k),
                format_ident!("__psync_out{}", chain_no),
            );
            let panic = format_ident!("__psync_panic{}", chain_no);
            chain_no += 1;
            let at = plan.release[c];
            let body = &code.body;
            // `maybe_done_at` is `maybe_done` plus what the analysis assumed, as const generics,
            // for `cargo psync check` to verify against the compiled callees.
            let order = proc_macro2::Literal::usize_unsuffixed(chain_positions[0]);
            let last = proc_macro2::Literal::usize_unsuffixed(*chain_positions.last().unwrap());
            let exits = members.iter().any(|n| n.exits);
            // Where the chain's first early exit is, in program order (0 when it has none).
            let exit_at = proc_macro2::Literal::usize_unsuffixed(
                members
                    .iter()
                    .zip(&chain_positions)
                    .find(|(n, _)| n.exits)
                    .map_or(0, |(_, p)| *p),
            );
            let spec = members.iter().any(|n| n.speculate);
            let indep = members.iter().any(|n| n.independent);
            let slot_idents = code.slot_idents();
            let taken = format_ident!("__psync_taken{}", k);
            if sync {
                // A job: the chain as a closure, run once, its panic caught with the member
                // that raised it.
                futures.push(quote!(let mut #f = ::core::option::Option::Some(
                    #krate::__private::job_at::<#level_id, #order, #last, #exits, #exit_at, #spec, #indep, _>(|| { #body })
                );));
                let nones = slot_idents
                    .iter()
                    .map(|_| quote!(::core::option::Option::None));
                takes.push(quote! {
                    let mut #panic = ::core::option::Option::None;
                    let (#(#slot_idents,)* #out_k) = match #taken {
                        (::core::result::Result::Ok(v), _) => v,
                        (::core::result::Result::Err(p), at) => {
                            #panic = ::core::option::Option::Some((at, p));
                            (#(#nones,)* ::core::option::Option::None)
                        }
                    };
                });
            } else {
                // Gated: wait for an earlier chain whose files turned out to overlap this one's,
                // and let later chains waiting for this one go when it ends.
                let waits = gates.iter().filter(|g| g.waits == c).map(|g| {
                    let (gate, cond) = (gate_of(g.on), &g.cond);
                    quote!(if #cond { #gate.wait().await; })
                });
                let opens = gates.iter().any(|g| g.on == c).then(|| {
                    let gate = gate_of(c);
                    quote!(#gate.open();)
                });
                let body = quote! {
                    #(#waits)*
                    let __psync_r = { #body };
                    #opens
                    __psync_r
                };
                futures.push(quote!(let mut #f = #krate::__private::maybe_done_at::<#level_id, #order, #last, #exits, #exit_at, #spec, #indep, _>(async { #body });));
                takes.push(quote!(let (#(#slot_idents,)* #out_k) = #taken;));
            }
            // In synchronous code a panic is replayed like an exit, at the member that raised it.
            let panic_at = |m: usize| {
                let m = proc_macro2::Literal::usize_unsuffixed(m);
                quote! {
                    if ::core::matches!(&#panic, ::core::option::Option::Some((#m, _))) {
                        ::std::panic::resume_unwind(#panic.take().unwrap().1);
                    }
                }
            };
            let pattern = code.binding_pattern();
            let bind = quote!(#[allow(unused_mut)] let #pattern = #out_k.unwrap(););
            for (m, (slots, pos)) in code.slots.iter().zip(chain_positions).enumerate() {
                let mut r = TokenStream::new();
                if sync {
                    r.extend(panic_at(m));
                }
                r.extend(slots.reraise(krate));
                if at == l {
                    reraises.push((pos, r));
                } else {
                    late_reraises.push((at, pos, r));
                }
            }
            if at == l {
                binds.push(bind);
            } else {
                late_binds.push((at, bind));
            }
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
        let progress: Vec<Ident> = (chain_no - level.len()..chain_no)
            .map(|c| format_ident!("__psync_at{}", c))
            .collect();
        // Synchronous code: the chains run as jobs, the first on this thread and the others on
        // psync's pool; each job's result and the member it reached are taken in an inner block,
        // so the closures' borrows end with it.
        let tokens = if sync {
            let results: Vec<Ident> = (0..level.len())
                .map(|k| format_ident!("__psync_r{}", k))
                .collect();
            quote! {{
                let (#(#taken,)*) = {
                    #(let #progress = ::core::sync::atomic::AtomicUsize::new(0);)*
                    #(#futures)*
                    #(let mut #results = ::core::option::Option::None;)*
                    #krate::__private::par_run(&mut [#(
                        &mut || {
                            #results = ::core::option::Option::Some(::std::panic::catch_unwind(
                                ::core::panic::AssertUnwindSafe(#fs.take().unwrap()),
                            ));
                        }
                    ),*]);
                    (#((
                        #results.unwrap(),
                        #progress.load(::core::sync::atomic::Ordering::Relaxed),
                    ),)*)
                };
                #(#takes)*
                #(#reraises)*
                #(#binds)*
            }}
        } else {
            // The futures live in an inner block: they (and whatever they borrow) are dropped as
            // soon as their outputs are taken, so later statements may move what they borrowed.
            quote! {{
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
            }}
        };
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
    fn visit_stmt_mut(&mut self, s: &mut Stmt) {
        if let Stmt::Expr(e, _) = s {
            strip_expr_hints(e);
        }
        syn::visit_mut::visit_stmt_mut(self, s);
    }
    fn visit_expr_while_mut(&mut self, w: &mut syn::ExprWhile) {
        self.visit_attributes_mut(&mut w.attrs);
        syn::visit_mut::visit_expr_while_mut(self, w);
    }
    fn visit_stmt_macro_mut(&mut self, m: &mut syn::StmtMacro) {
        self.visit_attributes_mut(&mut m.attrs);
    }
}

/// Removes hints from an expression statement's own attributes.
fn strip_expr_hints(e: &mut syn::Expr) {
    use syn::Expr::*;
    let attrs = match e {
        Try(t) => &mut t.attrs,
        Await(a) => &mut a.attrs,
        MethodCall(m) => &mut m.attrs,
        Call(c) => &mut c.attrs,
        If(i) => &mut i.attrs,
        Match(m) => &mut m.attrs,
        Assign(a) => &mut a.attrs,
        Block(b) => &mut b.attrs,
        _ => return,
    };
    attrs.retain(|a| !HINTS.iter().any(|h| is_psync_attr(a, h)));
}
