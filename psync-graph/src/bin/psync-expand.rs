//! `psync-expand FILE LINE [BOUND]`: apply `#[psync::graph]` to the innermost function containing
//! LINE of an existing Rust file and print the generated code and dependency graph as JSON.
//! Lets you see what psync would generate for code you have not annotated yet.
use psync_graph::{Options, transform_fn};
use syn::{ItemFn, spanned::Spanned, visit::Visit};

struct Find {
    line: usize,
    best: Option<(usize, ItemFn)>,
}
impl Find {
    fn consider(&mut self, f: ItemFn, span: proc_macro2::Span) {
        let (start, end) = (span.start().line, span.end().line);
        if start <= self.line
            && self.line <= end
            && self.best.as_ref().is_none_or(|(len, _)| end - start < *len)
        {
            self.best = Some((end - start, f));
        }
    }
}
impl<'ast> Visit<'ast> for Find {
    fn visit_item_fn(&mut self, f: &'ast ItemFn) {
        self.consider(f.clone(), f.span());
        syn::visit::visit_item_fn(self, f);
    }
    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        let item = ItemFn {
            attrs: f.attrs.clone(),
            vis: f.vis.clone(),
            sig: f.sig.clone(),
            block: Box::new(f.block.clone()),
        };
        self.consider(item, f.span());
        syn::visit::visit_impl_item_fn(self, f);
    }
    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        if let Some(block) = &f.default {
            let item = ItemFn {
                attrs: f.attrs.clone(),
                vis: syn::Visibility::Inherited,
                sig: f.sig.clone(),
                block: Box::new(block.clone()),
            };
            self.consider(item, f.span());
        }
        syn::visit::visit_trait_item_fn(self, f);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, file, line, rest @ ..] = args.as_slice() else {
        eprintln!("usage: psync-expand FILE LINE [BOUND]");
        std::process::exit(2);
    };
    let source = std::fs::read_to_string(file).expect("read file");
    let parsed = syn::parse_file(&source).expect("parse file");
    let mut find = Find {
        line: line.parse().expect("LINE"),
        best: None,
    };
    find.visit_file(&parsed);
    let Some((_, mut function)) = find.best else {
        eprintln!("no function contains line {line}");
        std::process::exit(1);
    };
    let (start, end) = (function.span().start().line, function.span().end().line);
    let mut options = Options::default();
    if let Some(b) = rest.first() {
        options.bound = b.parse().expect("BOUND");
    }
    let report = transform_fn(&mut function, &options);
    let changed =
        report.regions.iter().any(|r| r.changed) || report.loops.iter().any(|l| l.changed);
    let name = function.sig.ident.to_string();
    let expanded = prettyplease::unparse(&syn::File {
        shebang: None,
        attrs: Vec::new(),
        items: vec![syn::Item::Fn(function)],
    });
    let out = serde_json::json!({
        "function": name,
        "start_line": start,
        "end_line": end,
        "bound": options.bound,
        "changed": changed,
        "explain": psync_graph::explain(&report),
        "report": report,
        "expanded": expanded,
    });
    println!("{}", serde_json::to_string_pretty(&out).expect("json"));
}
