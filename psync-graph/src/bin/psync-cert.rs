//! `psync-cert [--facts FACTS]... [--expect-unsafe PREFIX]... FILE... > Certs.lean`: certificates
//! for every `#[psync::graph]` function in the given files, as a Lean module that
//! `verify/Cert.lean`'s checker verifies.
//!
//! The schedule is the one the macro builds from the function's tokens, as `cargo build` does.
//! The places are those tokens' places *and* what the compiler found the calls really do
//! (`--facts`, written by `cargo psync`; see `scripts/cert-facts.sh`): file and service effects of
//! callees, hidden state (`h/<static path>` for a `static` or `thread_local!`, `n/<x>/$state` for
//! a cell or lock behind the reference `x`, `h` for any), and panics. A certificate therefore
//! lists every place its statements touch that the compiler could see, and Lean decides whether
//! psync's schedule is still safe with them.
//!
//! State measured on purpose (`#[psync::observed]`, `observe(..)`) is listed and then hidden
//! (`Cert.hiding`): the theorem is that the schedule is equivalent on everything else, and a
//! second theorem shows whether it differs with the observed state counted. A schedule that is
//! unsafe with its places exits with status 1, unless its file starts with an `--expect-unsafe`
//! prefix (the driver fixture plants such schedules on purpose): then the module proves
//! `check cN = false`.
//!
//! For each region psync makes concurrent, it writes the certificate (each statement's places, and
//! the levels of chains psync chose) and a theorem `check cert = true`, proved by `decide`. If
//! Lean accepts the module, `check_sound` applies to every one of those schedules: each concurrent
//! execution equals the statements run in program order, given that the places describe the
//! statements. Place segments are interned to numbers in order of appearance.
use psync_graph::facts::{FactTable, use_facts};
use psync_graph::{CertNode, Options, Report, transform_fn};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use syn::{ItemFn, visit::Visit};

#[derive(Default)]
struct Collect(Vec<ItemFn>);

fn is_graph(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "graph"))
}

/// `graph_suite`'s `example! { .., options: (..), .., async fn .. }`: the function, with the
/// attribute the macro puts on it, `#[psync::graph(options)]`.
fn from_example(tokens: proc_macro2::TokenStream) -> Option<ItemFn> {
    use proc_macro2::TokenTree;
    let tts: Vec<TokenTree> = tokens.into_iter().collect();
    let mut options = proc_macro2::TokenStream::new();
    for w in tts.windows(3) {
        if let (TokenTree::Ident(i), TokenTree::Punct(p), TokenTree::Group(g)) =
            (&w[0], &w[1], &w[2])
            && i == "options"
            && p.as_char() == ':'
        {
            options = g.stream();
        }
    }
    let start = tts.windows(2).position(|w| {
        matches!((&w[0], &w[1]), (TokenTree::Ident(a), TokenTree::Ident(f)) if a == "async" && f == "fn")
    })?;
    let function: proc_macro2::TokenStream = tts[start..].iter().cloned().collect();
    syn::parse2(quote::quote!(#[psync::graph(#options)] #function)).ok()
}

impl<'ast> Visit<'ast> for Collect {
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if m.path.segments.last().is_some_and(|s| s.ident == "example")
            && let Some(f) = from_example(m.tokens.clone())
        {
            self.0.push(f);
        }
    }
    fn visit_item_fn(&mut self, f: &'ast ItemFn) {
        if is_graph(&f.attrs) {
            self.0.push(f.clone());
        }
        syn::visit::visit_item_fn(self, f);
    }
    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if is_graph(&f.attrs) {
            self.0.push(ItemFn {
                attrs: f.attrs.clone(),
                vis: f.vis.clone(),
                sig: f.sig.clone(),
                block: Box::new(f.block.clone()),
            });
        }
        syn::visit::visit_impl_item_fn(self, f);
    }
}

fn options(f: &ItemFn) -> Result<Options, syn::Error> {
    match f
        .attrs
        .iter()
        .find(|a| a.path().segments.last().is_some_and(|s| s.ident == "graph"))
        .map(|a| &a.meta)
    {
        Some(syn::Meta::List(l)) => Options::parse(l.tokens.clone()),
        _ => Ok(Options::default()),
    }
}

/// Segments to numbers; holes (`$hole:..`, a `format!` placeholder) are recorded per certificate.
struct Interner(HashMap<String, usize>, std::collections::BTreeSet<usize>);

impl Interner {
    fn place(&mut self, p: &[String]) -> String {
        let ids: Vec<String> = p
            .iter()
            .map(|s| {
                let n = self.0.len();
                let id = *self.0.entry(s.clone()).or_insert(n);
                if s.starts_with("$hole:") {
                    self.1.insert(id);
                }
                id.to_string()
            })
            .collect();
        format!("[{}]", ids.join(", "))
    }
    fn ids(&mut self, p: &[String]) -> Vec<usize> {
        p.iter()
            .map(|s| {
                let n = self.0.len();
                let id = *self.0.entry(s.clone()).or_insert(n);
                if s.starts_with("$hole:") {
                    self.1.insert(id);
                }
                id
            })
            .collect()
    }
    fn places(&mut self, ps: &[Vec<String>]) -> String {
        let items: Vec<String> = ps.iter().map(|p| self.place(p)).collect();
        format!("[{}]", items.join(", "))
    }
}

/// A certificate's node with interned places.
#[derive(Clone, Default)]
struct Ids {
    reads: Vec<Vec<usize>>,
    writes: Vec<Vec<usize>>,
}

/// `Cert.lean`'s `overlap`: agree until one ends, or first differ where either has a hole.
fn overlap(holes: &BTreeSet<usize>, a: &[usize], b: &[usize]) -> bool {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return holes.contains(x) || holes.contains(y);
        }
    }
    true
}

fn any_overlap(holes: &BTreeSet<usize>, xs: &[Vec<usize>], ys: &[Vec<usize>]) -> Option<(usize, usize)> {
    for (i, p) in xs.iter().enumerate() {
        for (j, q) in ys.iter().enumerate() {
            if overlap(holes, p, q) {
                return Some((i, j));
            }
        }
    }
    None
}

fn indep(holes: &BTreeSet<usize>, a: &Ids, b: &Ids) -> bool {
    let b_all: Vec<Vec<usize>> = b.reads.iter().chain(&b.writes).cloned().collect();
    any_overlap(holes, &a.writes, &b_all).is_none() && any_overlap(holes, &b.writes, &a.reads).is_none()
}

/// `Cert.lean`'s `check`, and the first pair of nodes it trips on.
fn check(holes: &BTreeSet<usize>, nodes: &[Ids], levels: &[Vec<Vec<usize>>]) -> Result<(), (usize, usize)> {
    let ind = |i: usize, j: usize| indep(holes, &nodes[i], &nodes[j]);
    for level in levels {
        for (a, ch) in level.iter().enumerate() {
            for ch2 in &level[a + 1..] {
                for &x in ch {
                    for &y in ch2 {
                        if !ind(x, y) {
                            return Err((x, y));
                        }
                    }
                }
            }
        }
    }
    let mut rest: Vec<usize> = levels.iter().flatten().flatten().copied().collect();
    for x in 0..nodes.len() {
        let Some(at) = rest.iter().position(|&y| y == x) else {
            return Err((x, x));
        };
        if let Some(&y) = rest[..at].iter().find(|&&y| !ind(y, x)) {
            return Err((y, x));
        }
        rest.remove(at);
    }
    if rest.is_empty() { Ok(()) } else { Err((rest[0], rest[0])) }
}

/// `Cert.hiding`: the node without the places under an observed place.
fn hide(n: &Ids, observed: &[Vec<usize>]) -> Ids {
    let keep = |p: &Vec<usize>| !observed.iter().any(|o| p.starts_with(o));
    Ids {
        reads: n.reads.iter().filter(|p| keep(p)).cloned().collect(),
        writes: n.writes.iter().filter(|p| keep(p)).cloned().collect(),
    }
}

/// Every place of `a`, and those of `b` it lacks.
fn union(a: &CertNode, b: &CertNode) -> CertNode {
    let mut out = a.clone();
    for (to, from) in [
        (&mut out.reads, &b.reads),
        (&mut out.writes, &b.writes),
        (&mut out.observed, &b.observed),
    ] {
        for p in from {
            if !to.contains(p) {
                to.push(p.clone());
            }
        }
    }
    out
}

fn main() {
    let mut files = Vec::new();
    let mut facts_text = String::new();
    let mut expect_unsafe: Vec<String> = Vec::new();
    // `--index FILE`: each certificate's function and its statements' source lines, so that other
    // tools can find the statements in compiled code (`scripts/charon/llbc2lean.py --cert`).
    let mut index_path: Option<String> = None;
    let mut index: Vec<serde_json::Value> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--facts" => {
                let f = args.next().expect("--facts FILE");
                facts_text += &std::fs::read_to_string(&f).expect("read facts");
            }
            "--expect-unsafe" => expect_unsafe.push(args.next().expect("--expect-unsafe PREFIX")),
            "--index" => index_path = Some(args.next().expect("--index FILE")),
            _ => files.push(a),
        }
    }
    if files.is_empty() {
        eprintln!("usage: psync-cert [--facts FACTS]... [--expect-unsafe PREFIX]... [--index FILE] FILE... > Certs.lean");
        std::process::exit(2);
    }
    let facts = (!facts_text.is_empty()).then(|| FactTable::parse(&facts_text));
    let mut out = String::from(
        "import Cert\n\n/-! Certificates generated by `psync-cert`; do not edit. -/\n\nnamespace PsyncCerts\nopen PsyncCert\n",
    );
    let mut interner = Interner(HashMap::new(), Default::default());
    let (mut count, mut failed) = (0usize, false);
    let (mut proved, mut hidden, mut rejected) = (0usize, 0usize, 0usize);
    for file in &files {
        let source = std::fs::read_to_string(file).expect("read file");
        let parsed = syn::parse_file(&source).expect("parse file");
        let mut collect = Collect::default();
        collect.visit_file(&parsed);
        let planted = expect_unsafe.iter().any(|p| file.starts_with(p.as_str()));
        for function in collect.0 {
            let name = function.sig.ident.to_string();
            let opts = match options(&function) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("{file}: `{name}`: {e}");
                    failed = true;
                    continue;
                }
            };
            // The schedule, from the tokens alone: what `cargo build` compiles.
            use_facts(None);
            let report: Report = transform_fn(&mut function.clone(), &opts);
            // The places, with the compiler's facts too.
            let with_facts: Option<Report> = facts.map(|t| {
                use_facts(Some((t, file.clone())));
                let r = transform_fn(&mut function.clone(), &opts);
                use_facts(None);
                r
            });
            for (r, region) in report.regions.iter().enumerate() {
                if !region.changed {
                    continue;
                }
                let id = format!("c{count}");
                interner.1.clear();
                count += 1;
                let factual = with_facts
                    .as_ref()
                    .and_then(|w| w.regions.get(r))
                    .filter(|w| w.cert.len() == region.cert.len());
                if with_facts.is_some() && factual.is_none() {
                    eprintln!("{file}: `{name}`, region {r}: the region has other statements with the compiler's facts");
                    failed = true;
                }
                let cert: Vec<CertNode> = match factual {
                    Some(w) => region.cert.iter().zip(&w.cert).map(|(a, b)| union(a, b)).collect(),
                    None => region.cert.clone(),
                };
                let nodes: Vec<String> = cert
                    .iter()
                    .map(|n| {
                        format!(
                            "    {{ reads := {}, writes := {} }}",
                            interner.places(&n.reads),
                            interner.places(&n.writes)
                        )
                    })
                    .collect();
                let ids: Vec<Ids> = cert
                    .iter()
                    .map(|n| Ids {
                        reads: n.reads.iter().map(|p| interner.ids(p)).collect(),
                        writes: n.writes.iter().map(|p| interner.ids(p)).collect(),
                    })
                    .collect();
                let mut observed: Vec<Vec<String>> = Vec::new();
                for n in &cert {
                    for p in &n.observed {
                        if !observed.contains(p) {
                            observed.push(p.clone());
                        }
                    }
                }
                let observed_ids: Vec<Vec<usize>> = observed.iter().map(|p| interner.ids(p)).collect();
                let levels_ids: Vec<Vec<Vec<usize>>> = region
                    .levels
                    .iter()
                    .map(|level| level.iter().map(|&c| region.chains[c].clone()).collect())
                    .collect();
                let levels: Vec<String> = levels_ids
                    .iter()
                    .map(|level| {
                        let chains: Vec<String> = level
                            .iter()
                            .map(|ch| {
                                let ns: Vec<String> = ch.iter().map(usize::to_string).collect();
                                format!("[{}]", ns.join(", "))
                            })
                            .collect();
                        format!("[{}]", chains.join(", "))
                    })
                    .collect();
                let holes = if interner.1.is_empty() {
                    String::new()
                } else {
                    let ids: Vec<String> = interner.1.iter().map(usize::to_string).collect();
                    format!("\n  holes := [{}]", ids.join(", "))
                };
                index.push(serde_json::json!({
                    "id": id,
                    "file": file,
                    "function": name,
                    "region": r,
                    "lines": region.nodes.iter().map(|n| [n.line, n.end_line]).collect::<Vec<_>>(),
                }));
                let strict = check(&interner.1, &ids, &levels_ids);
                let hidden_ids: Vec<Ids> = ids.iter().map(|n| hide(n, &observed_ids)).collect();
                let visible = check(&interner.1, &hidden_ids, &levels_ids);
                let (n, depth) = (nodes.len(), levels.len());
                let _ = write!(
                    out,
                    "\n/-- {file}: `{name}`, region {r} -/\ndef {id} : Cert where\n  nodes := [\n{}]\n  levels := [{}]{holes}\n",
                    nodes.join(",\n"),
                    levels.join(", ")
                );
                let why = |(i, j): (usize, usize)| -> String {
                    let names = |ps: &[Vec<String>]| -> String {
                        ps.iter().map(|p| p.join("/")).collect::<Vec<_>>().join(", ")
                    };
                    format!(
                        "n{i} (writes {}; reads {}) and n{j} (writes {}; reads {})",
                        names(&cert[i].writes),
                        names(&cert[i].reads),
                        names(&cert[j].writes),
                        names(&cert[j].reads)
                    )
                };
                match (strict, visible) {
                    (Ok(()), _) => {
                        proved += 1;
                        let _ = write!(
                            out,
                            "\n/-- $$\\operatorname{{check}}(\\mathtt{{{id}}}) = \\mathsf{{true}}$$\n\n\
                             The schedule psync generated for `{name}` ({file}, region {r}): {n} statements in {depth} \
                             level(s). By `PsyncCert.check_sound`, every concurrent execution of it equals running \
                             the {n} statements in program order. -/\n\
                             theorem {id}_ok : check {id} = true := by decide\n"
                        );
                    }
                    (Err(_), Ok(())) => {
                        hidden += 1;
                        let obs: Vec<String> = observed.iter().map(|p| p.join("/")).collect();
                        let _ = write!(
                            out,
                            "\n/-- The places of `{id}` that hold state measured on purpose: {}. -/\n\
                             def {id}_observed : List Place := {}\n\n\
                             /-- $$\\operatorname{{check}}(\\mathrm{{hiding}}(\\mathtt{{{id}}})) = \\mathsf{{true}}$$\n\n\
                             The schedule psync generated for `{name}` ({file}, region {r}): {n} statements in {depth} \
                             level(s). Every concurrent execution of it equals running the {n} statements in program \
                             order, on everything but the observed state. -/\n\
                             theorem {id}_ok : check ({id}.hiding {id}_observed) = true := by decide\n\n\
                             /-- $$\\operatorname{{check}}(\\mathtt{{{id}}}) = \\mathsf{{false}}$$\n\n\
                             With the observed state counted, the schedule is not equivalent: it changes that state \
                             as sequential code would not. -/\n\
                             theorem {id}_strict : check {id} = false := by decide\n",
                            obs.join(", "),
                            interner.places(&observed),
                        );
                    }
                    (Err(_), Err(pair)) => {
                        let target = if observed.is_empty() {
                            id.clone()
                        } else {
                            let _ = write!(
                                out,
                                "\ndef {id}_observed : List Place := {}\n",
                                interner.places(&observed)
                            );
                            format!("({id}.hiding {id}_observed)")
                        };
                        if planted {
                            rejected += 1;
                        } else {
                            eprintln!(
                                "{file}: `{name}`, region {r}: psync's schedule is unsafe with the places the compiler found: {}",
                                why(pair)
                            );
                            failed = true;
                        }
                        let _ = write!(
                            out,
                            "\n/-- $$\\operatorname{{check}}(\\mathtt{{{id}}}) = \\mathsf{{false}}$$\n\n\
                             The schedule psync generated for `{name}` ({file}, region {r}) is **not** safe: with \
                             what the compiler found its calls do, {}. This file is expected to hold such \
                             schedules (`--expect-unsafe`); `cargo psync check` reports them. -/\n\
                             theorem {id}_rejected : check {target} = false := by decide\n",
                            why(pair).replace('$', "\\$")
                        );
                    }
                }
            }
        }
    }
    out.push_str("\nend PsyncCerts\n");
    print!("{out}");
    if let Some(path) = index_path {
        let json = serde_json::to_string_pretty(&index).expect("index as JSON");
        std::fs::write(&path, json + "\n").expect("write the index");
    }
    eprintln!(
        "psync-cert: {count} certificate(s) from {} file(s): {proved} safe, {hidden} safe but for observed state, {rejected} rejected as expected{}",
        files.len(),
        match facts {
            Some(t) => format!(" ({} call facts)", t.len()),
            None => " (no compiler facts)".into(),
        }
    );
    if failed {
        std::process::exit(1);
    }
}
