//! Book, Part 0, "Syntax trees": what `syn` builds from tokens, and how to walk it.
//!
//! `cargo run -p psync-graph --example syntax_tree [-- "async fn f() { .. }"]`
//!
//! 1. Parse an async function into a `syn::ItemFn`.
//! 2. Print the shape of each statement's expression tree.
//! 3. Walk every statement with a `Visit` and collect what psync's access analysis starts from:
//!    the names a statement binds, the names it reads, and whether it awaits.
use std::collections::BTreeSet;
use syn::visit::{self, Visit};
use syn::{Expr, ItemFn, Pat, Stmt};

/// A one-line picture of an expression tree: the variant, then its children.
fn shape(e: &Expr) -> String {
    match e {
        Expr::Try(t) => format!("Try({})", shape(&t.expr)),
        Expr::Await(a) => format!("Await({})", shape(&a.base)),
        Expr::MethodCall(m) => {
            let args: Vec<String> = m.args.iter().map(shape).collect();
            format!(
                "MethodCall(.{}, receiver: {}, args: [{}])",
                m.method,
                shape(&m.receiver),
                args.join(", ")
            )
        }
        Expr::Call(c) => {
            let args: Vec<String> = c.args.iter().map(shape).collect();
            format!("Call({}, [{}])", shape(&c.func), args.join(", "))
        }
        Expr::Field(f) => {
            let member = &f.member;
            format!("Field({}, {})", shape(&f.base), quote::quote!(#member))
        }
        Expr::Reference(r) => format!("Ref({})", shape(&r.expr)),
        Expr::Path(p) => format!("Path({})", quote::quote!(#p).to_string().replace(' ', "")),
        Expr::Struct(s) => {
            let path = &s.path;
            format!("Struct({})", quote::quote!(#path))
        }
        Expr::Lit(_) => "Lit".into(),
        other => format!("<{}>", quote::quote!(#other)),
    }
}

/// Bindings, reads and awaits of one statement.
#[derive(Default)]
struct Access {
    binds: BTreeSet<String>,
    reads: BTreeSet<String>,
    awaits: usize,
}

impl<'ast> Visit<'ast> for Access {
    // A single-segment path in expression position is a variable being read: `id`, `user`.
    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        if let Some(name) = p.path.get_ident() {
            self.reads.insert(name.to_string());
        }
        visit::visit_expr_path(self, p);
    }
    // A `let` pattern binds names.
    fn visit_pat(&mut self, p: &'ast Pat) {
        if let Pat::Ident(i) = p {
            self.binds.insert(i.ident.to_string());
        }
        visit::visit_pat(self, p);
    }
    fn visit_expr_await(&mut self, a: &'ast syn::ExprAwait) {
        self.awaits += 1;
        visit::visit_expr_await(self, a);
    }
    // Shorthand fields (`Page { user, .. }`) read `user` without an `ExprPath`.
    fn visit_field_value(&mut self, f: &'ast syn::FieldValue) {
        if f.colon_token.is_none()
            && let syn::Member::Named(n) = &f.member
        {
            self.reads.insert(n.to_string());
        }
        visit::visit_field_value(self, f);
    }
}

fn main() {
    let default = r#"
        async fn profile_page(api: &Api, user_id: u64) -> Result<Page, Error> {
            let user = api.user(user_id).await?;
            let posts = api.posts(user_id).await?;
            let avatar = api.avatar(&user.avatar_url).await?;
            let friends = api.friends(user_id).await?;
            Ok(Page { user, posts, avatar, friends })
        }
    "#;
    let source = std::env::args()
        .nth(1)
        .unwrap_or_else(|| default.to_string());
    let function: ItemFn = syn::parse_str(&source).expect("an item fn");
    println!(
        "fn {} ({} statements, async: {})\n",
        function.sig.ident,
        function.block.stmts.len(),
        function.sig.asyncness.is_some()
    );
    for (n, stmt) in function.block.stmts.iter().enumerate() {
        let expr = match stmt {
            Stmt::Local(l) => &*l.init.as_ref().expect("initializer").expr,
            Stmt::Expr(e, _) => e,
            _ => continue,
        };
        let mut access = Access::default();
        access.visit_stmt(stmt);
        // A statement does not read what it binds.
        let reads: Vec<_> = access.reads.difference(&access.binds).cloned().collect();
        println!("s{n}: {}", shape(expr));
        println!(
            "    binds {:?}  reads {:?}  awaits {}",
            access.binds, reads, access.awaits
        );
    }
}
