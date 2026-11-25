//! `psync-scan [--sync|--async] FILE...`: apply `#[psync::graph]` with default options to every
//! function of the given files, as if each were annotated, and print one tab-separated line per
//! region where psync overlaps two waits: file, line, function, kind, number of statements, number
//! of levels in which two or more chains wait, and the first waiting statements. The kind is
//! `blocking` for a region of synchronous code (Phase II: its statements block), `async` for one
//! of awaits, also when it sits in an async block inside a synchronous function.
//!
//! It measures what psync finds *without hints*: in synchronous code, only statements that block
//! on the network or a process by path (`ureq`, `reqwest::blocking`, `TcpStream`, `Command`) are
//! candidates, so helpers that hide a request are not seen. A summary line goes to stderr.
use psync_graph::{Options, transform_fn};
use syn::{ItemFn, spanned::Spanned, visit::Visit};

#[derive(Default)]
struct Collect(Vec<(usize, ItemFn)>);

impl Collect {
    fn push(&mut self, f: ItemFn, span: proc_macro2::Span) {
        self.0.push((span.start().line, f));
    }
}

impl<'ast> Visit<'ast> for Collect {
    fn visit_item_fn(&mut self, f: &'ast ItemFn) {
        self.push(f.clone(), f.span());
        syn::visit::visit_item_fn(self, f);
    }
    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        let item = ItemFn {
            attrs: f.attrs.clone(),
            vis: f.vis.clone(),
            sig: f.sig.clone(),
            block: Box::new(f.block.clone()),
        };
        self.push(item, f.span());
        syn::visit::visit_impl_item_fn(self, f);
    }
}

fn main() {
    // Deeply nested real code overflows the default stack in `syn` and the analysis.
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(scan)
        .unwrap()
        .join()
        .unwrap();
}

fn scan() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let only = match args.first().map(String::as_str) {
        Some("--sync") => Some(true),
        Some("--async") => Some(false),
        _ => None,
    };
    if only.is_some() {
        args.remove(0);
    }
    let (mut files, mut functions, mut regions, mut found_fns) = (0usize, 0usize, 0usize, 0usize);
    for file in &args {
        let Ok(source) = std::fs::read_to_string(file) else {
            continue;
        };
        let Ok(parsed) = syn::parse_file(&source) else {
            continue;
        };
        files += 1;
        let mut collect = Collect::default();
        collect.visit_file(&parsed);
        for (line, mut f) in collect.0 {
            let sync = f.sig.asyncness.is_none();
            if only.is_some_and(|s| s != sync) || f.sig.constness.is_some() {
                continue;
            }
            functions += 1;
            let name = f.sig.ident.to_string();
            // A panic in the analysis is a psync bug: recorded, and the scan goes on (in a build
            // that unwinds; psync's release profile aborts).
            let Ok(report) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transform_fn(&mut f, &Options::default())
            })) else {
                println!("{file}\t{line}\t{name}\tPANIC\t0\t0\t");
                continue;
            };
            let mut any = false;
            for region in report.regions.iter().filter(|r| r.changed) {
                // Useful only if two waiting statements (awaits, or blocking calls) share a level
                // in different chains: overlapping a wait with a `Vec::new()` gains nothing.
                let waits = |c: &usize| region.chains[*c].iter().any(|&n| region.nodes[n].awaits);
                let wide = region
                    .levels
                    .iter()
                    .filter(|l| l.iter().filter(|c| waits(c)).count() > 1)
                    .count();
                if wide == 0 {
                    continue;
                }
                any = true;
                regions += 1;
                let labels: Vec<String> = region
                    .nodes
                    .iter()
                    .filter(|n| n.awaits)
                    .take(4)
                    .map(|n| n.label.replace(['\t', '\n'], " "))
                    .collect();
                let blocking = region.nodes.iter().any(|n| n.blocks);
                println!(
                    "{file}\t{line}\t{name}\t{}\t{}\t{wide}\t{}",
                    if blocking { "blocking" } else { "async" },
                    region.nodes.len(),
                    labels.join(" | ")
                );
            }
            found_fns += any as usize;
        }
    }
    eprintln!(
        "psync-scan: {files} files, {functions} functions, {found_fns} with a concurrent region ({regions} regions)"
    );
}
