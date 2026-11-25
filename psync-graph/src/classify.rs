//! Statement classification: which statements become graph nodes, and what each one does.
//!
//! A *node* is a statement psync may move: a binding of an awaited or pure value, an awaited
//! expression statement, or an ordinary statement (an *effect*, kept in order with other effects).
//! A *barrier* stays exactly where it is and splits the block into independent regions.
//! Early exits (`?`, `return`, `break`, `continue`) inside a node are allowed: code generation
//! records them and re-raises them in program order (see [`crate::exits`]).

use crate::access::{Access, PURE_METHODS, Set, scan};
use crate::effects::{self, Effect};
use proc_macro2::{Delimiter, Ident, Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::{Attribute, Expr, Local, LocalInit, Pat, Stmt, spanned::Spanned, visit::Visit};

/// Mutability facts about the enclosing function, needed to tell reads from writes.
pub(crate) struct Scope {
    /// `mut` bindings anywhere in the function.
    pub mutable: Set,
    /// Bindings that hold `&mut` values, including `&mut self`.
    pub handles: Set,
    /// `#[psync::fresh]` bindings and their initializers: each node that uses one evaluates the
    /// initializer again instead of sharing the value (for example `&mut context.pool()`).
    /// Keyed by name; holds the binding's own identifier (for hygiene) and its initializer.
    pub fresh: std::collections::BTreeMap<String, (Ident, TokenStream)>,
    /// Effect settings from the attribute (`strict`, `speculate`, `effects(..)`).
    pub effects: effects::Config,
    /// A synchronous function (Phase II): statements that block on the outside world, or carry
    /// `#[psync::par]`, play the part of awaits, and levels run on threads.
    pub sync: bool,
}

/// A movable statement and what the analysis knows about it.
pub(crate) struct Node {
    /// The statement as written.
    pub stmt: Stmt,
    /// Bindings it introduces, with their `mut` flag.
    pub defs: Vec<(Ident, bool)>,
    /// What it reads and writes.
    pub acc: Access,
    /// It awaits (outside nested closures and async blocks), or, in synchronous code, blocks.
    pub awaits: bool,
    /// Synchronous code: it blocks on the network or a process, or carries `#[psync::par]`.
    pub blocks: bool,
    /// An ordinary statement with possible side effects: it keeps its order relative to other
    /// effects, and if it can exit early, later nodes wait for it.
    pub effect: bool,
    /// It contains `?`, `return`, `break` or `continue` that leave the statement.
    pub exits: bool,
    /// What it does to files, the network, services, processes and the environment.
    pub effects: Vec<Effect>,
    /// Awaited calls whose effect psync could not tell from their names.
    pub unclassified: Vec<String>,
    /// `#[psync::speculate]`: its writes may run ahead of earlier exits.
    pub speculate: bool,
    /// The compiler says it may panic on purpose: an exit for the speculation rule.
    pub panics: bool,
    /// `#[psync::independent]` on the statement: its effects commute with the others in its
    /// region (two writes to different objects through one client).
    pub independent: bool,
    /// Short description for reports.
    pub label: String,
    /// Source line (0 when spans carry no location).
    pub line: usize,
}

/// The outcome of classifying one statement.
pub(crate) enum Class {
    Node(Box<Node>),
    /// Stays in place. Carries a reason when the statement awaits, for reports.
    Barrier(Option<String>),
}

/// Awaited operations that acquire something held afterwards (a lock, permit or transaction).
/// A statement that keeps the guard is a barrier; one that drops it inside a nested block (its own
/// critical section) may still move, since the lock itself keeps mutual exclusion.
const GUARD_AWAITS: &[&str] = &[
    "lock",
    "lock_owned",
    "read",
    "write",
    "acquire",
    "acquire_owned",
    "acquire_many",
    "begin",
    "transaction",
];

/// Deliberate delays: their position is the point. Statements awaiting these are barriers.
const DELAY_AWAITS: &[&str] = &["sleep", "sleep_until", "tick", "yield_now", "delay_for"];

/// Clock reads (`Instant::now()`, `started.elapsed()`): when they run relative to the waits
/// around them is what they measure, so statements making them are barriers.
const CLOCK_CALLS: &[&str] = &["now", "elapsed", "duration_since"];

/// Whether `tokens` call a clock function outside closures and async blocks (`f(..)`, `x.f(..)`).
fn reads_clock(tokens: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    tokens.iter().enumerate().any(|(i, t)| match t {
        TokenTree::Ident(id) => {
            CLOCK_CALLS.contains(&id.to_string().as_str())
                && matches!(tokens.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
        }
        // A closure or async block decides for itself when it runs.
        TokenTree::Group(g) => {
            let deferred = tokens[..i].iter().rev().take(2).any(|p| {
                matches!(p, TokenTree::Punct(c) if c.as_char() == '|')
                    || matches!(p, TokenTree::Ident(k) if k == "async" || k == "move")
            });
            !deferred && reads_clock(g.stream())
        }
        _ => false,
    })
}

/// Synchronous calls that wait on the network or a process (Phase II): in a synchronous function,
/// a statement making one plays the part of an await. A statement blocks when it names one of
/// these paths *and* calls one of the methods that wait: `Command::new("git")` builds a command,
/// `.output()` runs it, and `Command::Reply` or clap's `Command::parse_from` are no process at all.
/// `reqwest` counts only through `reqwest::blocking`.
const NET_PATHS: &[&str] = &["ureq", "TcpStream", "UdpSocket"];
const NET_WAITS: &[&str] = &[
    "connect",
    "call",
    "send",
    "send_to",
    "recv",
    "recv_from",
    "read_to_end",
    "read_to_string",
    "read_exact",
];
const PROCESS_WAITS: &[&str] = &["output", "status", "wait", "wait_with_output"];

/// Methods whose result, bound by a `let`, is a guard held afterwards (synchronous locks and
/// `RefCell` borrows; `read` and `write` only without arguments, as on `RwLock`).
const GUARD_METHODS: &[&str] = &["lock", "try_lock", "borrow", "borrow_mut", "read", "write"];

/// Scans tokens outside closures and async blocks, which decide for themselves when they run,
/// and outside macro arguments, whose effects psync cannot see (`assert!(cmd.status()?..)` must
/// not become a blocking statement with no effects).
fn any_token(tokens: TokenStream, f: &dyn Fn(&[TokenTree], usize) -> bool) -> bool {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    tokens.iter().enumerate().any(|(i, t)| match t {
        TokenTree::Group(g) => {
            let deferred = tokens[..i].iter().rev().take(2).any(|p| {
                matches!(p, TokenTree::Punct(c) if c.as_char() == '|')
                    || matches!(p, TokenTree::Ident(k) if k == "async" || k == "move")
            });
            let macro_args =
                i > 0 && matches!(&tokens[i - 1], TokenTree::Punct(c) if c.as_char() == '!');
            !deferred && !macro_args && any_token(g.stream(), f)
        }
        _ => f(&tokens, i),
    })
}

/// A synchronous call to the network or a process: a path that reaches one, and a method that
/// waits for it.
fn blocks_on_world(tokens: TokenStream) -> bool {
    let named = |set: &'static [&'static str]| {
        move |ts: &[TokenTree], i: usize| match &ts[i] {
            TokenTree::Ident(id) if set.contains(&id.to_string().as_str()) => true,
            TokenTree::Ident(id) if id == "reqwest" && set == NET_PATHS => {
                matches!(ts.get(i + 3), Some(TokenTree::Ident(b)) if b == "blocking")
            }
            _ => false,
        }
    };
    // `.output()` and `.call()`, and for the network also `TcpStream::connect(..)`.
    let calls = |set: &'static [&'static str]| {
        move |ts: &[TokenTree], i: usize| {
            matches!(&ts[i], TokenTree::Ident(m) if set.contains(&m.to_string().as_str()))
                && i > 0
                && matches!(&ts[i - 1], TokenTree::Punct(p)
                    if p.as_char() == '.' || (p.as_char() == ':' && set == NET_WAITS))
                && matches!(ts.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis)
        }
    };
    let net = any_token(tokens.clone(), &named(NET_PATHS))
        && any_token(tokens.clone(), &calls(NET_WAITS));
    let process = any_token(
        tokens.clone(),
        &|ts, i| matches!(&ts[i], TokenTree::Ident(id) if id == "Command"),
    ) && any_token(tokens, &calls(PROCESS_WAITS));
    net || process
}

/// `.lock()`, `.borrow_mut()`, `.read()` and the like: the value may be a guard.
fn takes_guard(tokens: TokenStream) -> bool {
    any_token(tokens, &|ts, i| {
        matches!(
            (&ts[i], ts.get(i + 1), i.checked_sub(1).map(|p| &ts[p])),
            (TokenTree::Ident(m), Some(TokenTree::Group(g)), Some(TokenTree::Punct(dot)))
                if dot.as_char() == '.'
                    && g.delimiter() == Delimiter::Parenthesis
                    && g.stream().is_empty()
                    && GUARD_METHODS.contains(&m.to_string().as_str())
        )
    })
}

/// Logging statements: treated as pure so they do not split regions.
const LOG_MACROS: &[&str] = &["trace", "debug", "info", "warn", "error", "log"];

/// Macros that build values without side effects.
const PURE_MACROS: &[&str] = &["format", "vec", "concat", "stringify"];

/// Finds awaits and early exits in an expression, without entering closures, async blocks or
/// nested items (their awaits and exits belong to them).
#[derive(Default)]
pub(crate) struct Exits {
    /// Labels of loops and blocks inside the expression; exits to them stay inside.
    local_labels: Vec<String>,
    /// Depth of unlabeled loops inside the expression.
    loop_depth: usize,
    pub awaits: bool,
    pub exits: bool,
    /// The awaited method or function names, to spot ordering-sensitive awaits.
    pub awaited: Vec<String>,
    /// The same names, only for awaits outside any nested block (a guard taken in a nested
    /// block is released before the statement ends).
    pub awaited_unscoped: Vec<String>,
    /// Nesting depth of blocks inside the expression.
    block_depth: usize,
    /// Why the expression cannot be rewritten (an exit hidden in a macro, `yield`, ...).
    pub blocker: Option<String>,
}

impl Exits {
    pub fn of(e: &Expr) -> Self {
        let mut x = Exits::default();
        x.visit_expr(e);
        x
    }
    pub fn of_stmt(s: &Stmt) -> Self {
        let mut x = Exits::default();
        x.visit_stmt(s);
        x
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
    fn in_loop(&mut self, label: Option<&syn::Label>, f: impl FnOnce(&mut Self)) {
        match label {
            Some(l) => {
                self.local_labels.push(l.name.ident.to_string());
                self.loop_depth += 1;
                f(self);
                self.loop_depth -= 1;
                self.local_labels.pop();
            }
            None => {
                self.loop_depth += 1;
                f(self);
                self.loop_depth -= 1;
            }
        }
    }
}

impl<'ast> Visit<'ast> for Exits {
    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
    fn visit_item(&mut self, _: &'ast syn::Item) {}
    fn visit_block(&mut self, b: &'ast syn::Block) {
        self.block_depth += 1;
        syn::visit::visit_block(self, b);
        self.block_depth -= 1;
    }
    fn visit_expr_await(&mut self, e: &'ast syn::ExprAwait) {
        self.awaits = true;
        self.awaited.push(callee(&e.base));
        if self.block_depth == 0 {
            self.awaited_unscoped.push(callee(&e.base));
        }
        syn::visit::visit_expr_await(self, e);
    }
    fn visit_expr_try(&mut self, e: &'ast syn::ExprTry) {
        self.exits = true;
        syn::visit::visit_expr_try(self, e);
    }
    fn visit_expr_return(&mut self, e: &'ast syn::ExprReturn) {
        self.exits = true;
        syn::visit::visit_expr_return(self, e);
    }
    fn visit_expr_yield(&mut self, e: &'ast syn::ExprYield) {
        self.blocker.get_or_insert_with(|| "`yield`".into());
        syn::visit::visit_expr_yield(self, e);
    }
    fn visit_expr_break(&mut self, e: &'ast syn::ExprBreak) {
        if self.leaves(e.label.as_ref()) {
            if e.expr.is_some() {
                self.blocker
                    .get_or_insert_with(|| "`break` with a value".into());
            }
            self.exits = true;
        }
        syn::visit::visit_expr_break(self, e);
    }
    fn visit_expr_continue(&mut self, e: &'ast syn::ExprContinue) {
        if self.leaves(e.label.as_ref()) {
            self.exits = true;
        }
    }
    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.visit_expr(&e.expr);
        self.in_loop(e.label.as_ref(), |x| x.visit_block(&e.body));
    }
    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        self.visit_expr(&e.cond);
        self.in_loop(e.label.as_ref(), |x| x.visit_block(&e.body));
    }
    fn visit_expr_loop(&mut self, e: &'ast syn::ExprLoop) {
        self.in_loop(e.label.as_ref(), |x| x.visit_block(&e.body));
    }
    fn visit_expr_block(&mut self, e: &'ast syn::ExprBlock) {
        if let Some(l) = &e.label {
            self.local_labels.push(l.name.ident.to_string());
            self.visit_block(&e.block);
            self.local_labels.pop();
        } else {
            self.visit_block(&e.block);
        }
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // Exits inside macro tokens cannot be rewritten, so they block the statement.
        fn walk(ts: TokenStream, awaits: &mut bool, exits: &mut bool) {
            for tt in ts {
                match tt {
                    TokenTree::Group(g) => walk(g.stream(), awaits, exits),
                    TokenTree::Ident(i) if i == "await" => *awaits = true,
                    TokenTree::Ident(i) if i == "return" => *exits = true,
                    TokenTree::Punct(p) if p.as_char() == '?' => *exits = true,
                    _ => {}
                }
            }
        }
        let name = macro_name(m);
        let (mut awaits, mut exits) = (false, false);
        walk(m.tokens.clone(), &mut awaits, &mut exits);
        self.awaits |= awaits;
        // Logging macros use `?x` and `%x` as field sigils, not as operators.
        let exits = exits && !LOG_MACROS.contains(&name.as_str());
        if exits
            || matches!(
                name.as_str(),
                "bail" | "ensure" | "try" | "return_err" | "fail"
            )
        {
            self.blocker
                .get_or_insert_with(|| format!("`{name}!` may return early"));
        }
    }
}

pub(crate) fn macro_name(m: &syn::Macro) -> String {
    m.path
        .segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}

/// The name of an awaited call: `method` for `x.method(..)`, `Type::f` for `Type::f(..)`.
fn callee(e: &Expr) -> String {
    match e {
        Expr::MethodCall(m) => m.method.to_string(),
        Expr::Call(c) => match &*c.func {
            Expr::Path(p) => {
                let segs: Vec<String> = p
                    .path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect();
                segs[segs.len().saturating_sub(2)..].join("::")
            }
            _ => "call".into(),
        },
        Expr::Macro(m) => format!("{}!", macro_name(&m.mac)),
        Expr::Paren(p) => callee(&p.expr),
        Expr::Try(t) => callee(&t.expr),
        other => other
            .to_token_stream()
            .to_string()
            .chars()
            .take(18)
            .collect(),
    }
}

/// Cheap expressions without side effects, which may move freely relative to awaits.
pub(crate) fn is_pure(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Path(_) | Expr::Closure(_) | Expr::Range(_) => true,
        Expr::Field(f) => is_pure(&f.base),
        Expr::Reference(r) => r.mutability.is_none() && is_pure(&r.expr),
        Expr::Paren(p) => is_pure(&p.expr),
        Expr::Group(g) => is_pure(&g.expr),
        Expr::Unary(u) => is_pure(&u.expr),
        Expr::Binary(b) => is_pure(&b.left) && is_pure(&b.right),
        Expr::Cast(c) => is_pure(&c.expr),
        Expr::Index(i) => is_pure(&i.expr) && is_pure(&i.index),
        Expr::Tuple(t) => t.elems.iter().all(is_pure),
        Expr::Array(a) => a.elems.iter().all(is_pure),
        Expr::Struct(s) => {
            s.fields.iter().all(|f| is_pure(&f.expr)) && s.rest.as_ref().is_none_or(|r| is_pure(r))
        }
        // `if arch == "i686" { "i386" } else { arch }`: a choice between pure values.
        Expr::If(i) => {
            is_pure(&i.cond)
                && pure_block(&i.then_branch)
                && i.else_branch.as_ref().is_some_and(|(_, e)| match &**e {
                    Expr::Block(b) => pure_block(&b.block),
                    e => is_pure(e),
                })
        }
        Expr::Block(b) if b.label.is_none() => pure_block(&b.block),
        Expr::MethodCall(m) => {
            // `path.join(name)`, `parts.join(", ")`; `handle.join()` (a thread) takes no argument.
            (PURE_METHODS.contains(&m.method.to_string().as_str())
                || (m.method == "join" && m.args.len() == 1))
                && is_pure(&m.receiver)
                && m.args.iter().all(is_pure)
        }
        Expr::Call(c) => {
            // Constructors only: `T::new`, `T::from`, `Some(..)`, tuple structs.
            let Expr::Path(p) = &*c.func else {
                return false;
            };
            let last = p
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            let ctor = matches!(
                last.as_str(),
                "new" | "from" | "default" | "with_capacity" | "Some" | "Ok" | "Err"
            ) || (p.path.segments.len() == 1 && last.starts_with(char::is_uppercase));
            ctor && c.args.iter().all(is_pure)
        }
        Expr::Macro(m) => {
            let e = Exits::of(&Expr::Macro(m.clone()));
            PURE_MACROS.contains(&macro_name(&m.mac).as_str()) && !e.awaits && e.blocker.is_none()
        }
        _ => false,
    }
}

/// A block that is a single pure expression.
fn pure_block(b: &syn::Block) -> bool {
    matches!(&b.stmts[..], [Stmt::Expr(e, None)] if is_pure(e))
}

/// A place expression whose evaluation has no side effects (a variable, field or pure index).
pub(crate) fn is_place(e: &Expr) -> bool {
    match e {
        Expr::Path(_) => true,
        Expr::Field(f) => is_place(&f.base),
        Expr::Index(i) => is_place(&i.expr) && is_pure(&i.index),
        Expr::Paren(p) => is_place(&p.expr),
        Expr::Unary(u) => matches!(u.op, syn::UnOp::Deref(_)) && is_place(&u.expr),
        _ => false,
    }
}

/// Bindings introduced by a pattern, with their `mut` flag.
pub(crate) fn pat_idents(pat: &Pat, out: &mut Vec<(Ident, bool)>) {
    struct V<'a>(&'a mut Vec<(Ident, bool)>);
    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
            self.0.push((p.ident.clone(), p.mutability.is_some()));
            syn::visit::visit_pat_ident(self, p);
        }
        // Enum variants and constants in patterns are not bindings.
        fn visit_path(&mut self, _: &'ast syn::Path) {}
    }
    V(out).visit_pat(pat);
}

/// True when a pattern binds by reference (`ref x`), which cannot be handed between scopes.
pub(crate) fn pat_has_ref(pat: &Pat) -> bool {
    struct V(bool);
    impl<'ast> Visit<'ast> for V {
        fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
            self.0 |= p.by_ref.is_some();
            syn::visit::visit_pat_ident(self, p);
        }
    }
    let mut v = V(false);
    v.visit_pat(pat);
    v.0
}

/// The hint attributes psync reads and removes from its output.
pub(crate) const HINTS: &[&str] = &["seq", "fresh", "independent", "speculate", "par"];

/// The attributes of an expression statement (`#[psync::speculate] api.delete(x).await?;`).
pub(crate) fn expr_attrs(e: &Expr) -> &[Attribute] {
    match e {
        Expr::Try(t) => &t.attrs,
        Expr::Await(a) => &a.attrs,
        Expr::MethodCall(m) => &m.attrs,
        Expr::Call(c) => &c.attrs,
        Expr::If(i) => &i.attrs,
        Expr::Match(m) => &m.attrs,
        Expr::ForLoop(f) => &f.attrs,
        Expr::While(w) => &w.attrs,
        Expr::Assign(a) => &a.attrs,
        Expr::Block(b) => &b.attrs,
        _ => &[],
    }
}

/// True for `#[psync::name]` (or a bare `#[graph]`).
pub(crate) fn is_psync_attr(attr: &Attribute, name: &str) -> bool {
    let segs: Vec<String> = attr
        .path()
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect();
    segs.last().is_some_and(|l| l == name)
        && (segs.len() == 1 && name == "graph" || segs.first().is_some_and(|f| f == "psync"))
}

/// Last source line of a span (see [`line_of`]).
pub(crate) fn end_line_of(span: Span) -> usize {
    #[cfg(feature = "cli")]
    {
        span.end().line
    }
    #[cfg(not(feature = "cli"))]
    {
        let _ = span;
        0
    }
}

/// Source line of a span; only known when proc-macro2 tracks locations (the `cli` feature).
pub(crate) fn line_of(span: Span) -> usize {
    #[cfg(feature = "cli")]
    {
        span.start().line
    }
    #[cfg(not(feature = "cli"))]
    {
        let _ = span;
        0
    }
}

/// Every name bound by a pattern anywhere inside `block`.
pub(crate) fn bindings_in(block: &syn::Block) -> Set {
    struct V(Set);
    impl<'ast> Visit<'ast> for V {
        fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
            self.0.insert(p.ident.to_string());
            syn::visit::visit_pat_ident(self, p);
        }
    }
    let mut v = V(Set::new());
    v.visit_block(block);
    v.0
}

/// Every name bound by a pattern anywhere inside `stmt`, except the statement's own bindings.
fn inner_bindings(stmt: &Stmt) -> Set {
    struct V(Set);
    impl<'ast> Visit<'ast> for V {
        fn visit_pat_ident(&mut self, p: &'ast syn::PatIdent) {
            self.0.insert(p.ident.to_string());
            syn::visit::visit_pat_ident(self, p);
        }
    }
    let mut v = V(Set::new());
    match stmt {
        Stmt::Local(l) => {
            if let Some(init) = &l.init {
                v.visit_expr(&init.expr);
                if let Some((_, d)) = &init.diverge {
                    v.visit_expr(d);
                }
            }
        }
        other => v.visit_stmt(other),
    }
    v.0
}

/// Short label for reports: the bindings and the first awaited call.
fn label_for(defs: &[(Ident, bool)], stmt: &Stmt, x: &Exits) -> String {
    let rhs = match x.awaited.first() {
        Some(c) if c.ends_with('!') => c.clone(),
        Some(c) => format!("{c}()"),
        None => match stmt {
            Stmt::Macro(m) => format!("{}!", macro_name(&m.mac)),
            // A binding of a computed value: show the value, not the whole statement.
            Stmt::Local(Local {
                init: Some(init), ..
            }) => {
                let text = init.expr.to_token_stream().to_string();
                match text.char_indices().nth(23) {
                    Some((cut, _)) => format!("{}…", &text[..cut]),
                    None => text,
                }
            }
            _ => stmt
                .to_token_stream()
                .to_string()
                .chars()
                .take(24)
                .collect(),
        },
    };
    let rhs = if x.exits { format!("{rhs}?") } else { rhs };
    if defs.is_empty() {
        rhs
    } else {
        let names: Vec<String> = defs.iter().map(|(i, _)| i.to_string()).collect();
        format!("{} = {rhs}", names.join(", "))
    }
}

/// Classify one statement.
///
/// `relaxed` admits bindings of non-pure, non-awaited values as movable (not effects); it is used
/// for a loop iteration's leading statements, which the caller confines to per-iteration data.
pub(crate) fn classify(stmt: &Stmt, scope: &Scope, relaxed: bool) -> Class {
    let seq = |attrs: &[Attribute]| attrs.iter().any(|a| is_psync_attr(a, "seq"));
    let x = Exits::of_stmt(stmt);
    let describe = |why: &str| {
        x.awaits.then(|| {
            let text: String = stmt
                .to_token_stream()
                .to_string()
                .chars()
                .take(60)
                .collect();
            format!("line {}: `{text}`: {why}", line_of(stmt.span()))
        })
    };
    // What kind of statement this is: `(attributes, pattern, value)` for bindings, and whether
    // it is a binding/await of a value (`shaped`) or an ordinary statement.
    let (attrs, pat, value, shaped): (&[Attribute], Option<&Pat>, Option<&Expr>, bool) = match stmt
    {
        Stmt::Local(Local {
            attrs,
            pat,
            init: Some(LocalInit { expr, .. }),
            ..
        }) => (attrs, Some(pat), Some(expr), true),
        Stmt::Local(_) => return Class::Barrier(None),
        // Block-like statements (`if`, `match`, `for` without `;`) carry no semicolon even in the
        // middle of a block; the caller keeps a block's final value expression in place.
        Stmt::Expr(e, _) => {
            let awaited = match e {
                Expr::Await(_) => true,
                Expr::Try(t) => matches!(&*t.expr, Expr::Await(_)),
                _ => false,
            };
            (expr_attrs(e), None, Some(e), awaited)
        }
        Stmt::Macro(m) => (
            &m.attrs,
            None,
            None,
            LOG_MACROS.contains(&macro_name(&m.mac).as_str()),
        ),
        Stmt::Item(_) => return Class::Barrier(None),
    };
    if seq(attrs) {
        return Class::Barrier(describe("marked #[psync::seq]"));
    }
    // A `#[psync::fresh]` binding stays where it is; nodes re-create it instead of sharing it.
    if attrs.iter().any(|a| is_psync_attr(a, "fresh")) {
        return Class::Barrier(None);
    }
    if let Some(why) = &x.blocker {
        return Class::Barrier(describe(why));
    }
    if reads_clock(stmt.to_token_stream()) {
        return Class::Barrier(describe("reads the clock, whose position is meaningful"));
    }
    if pat.is_some_and(pat_has_ref) {
        return Class::Barrier(describe("`ref` binding"));
    }
    let delay = x
        .awaited
        .iter()
        .find(|m| DELAY_AWAITS.contains(&m.as_str()));
    let guard = x
        .awaited_unscoped
        .iter()
        .find(|m| GUARD_AWAITS.contains(&m.as_str()));
    if let Some(m) = delay.or(guard) {
        return Class::Barrier(describe(&format!(
            "awaits `{m}`, whose position is meaningful"
        )));
    }
    // Phase II: in synchronous code a blocking statement is what overlaps, as an await is in
    // async code, and a guard held by a binding keeps its place as an awaited lock does.
    let blocks = scope.sync
        && (attrs.iter().any(|a| is_psync_attr(a, "par"))
            || blocks_on_world(stmt.to_token_stream()));
    if scope.sync
        && let Stmt::Local(l) = stmt
        && l.init
            .as_ref()
            .is_some_and(|i| takes_guard(i.expr.to_token_stream()))
    {
        return Class::Barrier(None);
    }
    let awaits = x.awaits || blocks;
    let shaped = shaped || blocks;
    let pure_value = value.is_some_and(is_pure) || matches!(stmt, Stmt::Macro(_));
    let mut effect = !shaped || (!awaits && !relaxed && !pure_value);
    let mut defs = Vec::new();
    if let Some(p) = pat {
        pat_idents(p, &mut defs);
    }
    let mut acc = Access::default();
    let tokens = match (stmt, value) {
        (Stmt::Local(l), _) => {
            let mut tokens = TokenStream::new();
            if let Some(init) = &l.init {
                tokens.extend(init.expr.to_token_stream());
                if let Some((_, diverge)) = &init.diverge {
                    tokens.extend(diverge.to_token_stream());
                }
            }
            tokens
        }
        (Stmt::Macro(m), _) => m.mac.tokens.clone(),
        (_, Some(v)) => v.to_token_stream(),
        _ => TokenStream::new(),
    };
    scan(tokens, &scope.mutable, &scope.handles, &mut acc);
    // Variables bound inside the statement (match arms, nested `let`s) are its own business.
    let inner = inner_bindings(stmt);
    acc.wpaths
        .retain(|p| !inner.contains(crate::access::root(p)));
    acc.writes.retain(|w| !inner.contains(w));
    // `#[psync::fresh]` bindings are re-created for each node, so using one is no conflict.
    acc.wpaths
        .retain(|p| !scope.fresh.contains_key(crate::access::root(p)));
    acc.rpaths
        .retain(|p| !scope.fresh.contains_key(crate::access::root(p)));
    if relaxed && !shaped {
        // In a loop prefix, an ordinary statement may join only if it writes nothing (a guard
        // like `if done { continue; }`); its exits are replayed in order like any other.
        if !acc.wpaths.is_empty() || matches!(stmt, Stmt::Macro(_)) {
            return Class::Barrier(None);
        }
        effect = false;
    }
    let mut found = effects::of_stmt_in(stmt, &scope.effects, scope.sync);
    // A blocking statement whose calls psync cannot classify is what an unclassified awaited call
    // is in async code: reported, and under `strict` a write to anything.
    if blocks && found.effects.is_empty() {
        let mut v = value;
        while let Some(e) = v {
            v = match e {
                Expr::Try(t) => Some(&*t.expr),
                Expr::MethodCall(m)
                    if matches!(m.method.to_string().as_str(), "unwrap" | "expect") =>
                {
                    Some(&*m.receiver)
                }
                _ => break,
            };
        }
        let op = v.map_or_else(|| "statement".to_string(), callee);
        if scope.effects.strict {
            found.effects.push(effects::Effect {
                domain: effects::Domain::Unknown,
                kind: effects::Kind::Write,
                target: effects::Target::Any,
                op: op.clone(),
                ordered: true,
            });
        }
        found.unclassified.push(op);
    }
    // A receiver bound inside the statement (`let mut s = TcpStream::connect(..)?; s.write_all(..)`)
    // is that statement's own: different receivers are different services, so it is named
    // apart from another statement's `s`. Identical statements keep the same name, and conflict.
    for e in &mut found.effects {
        if let effects::Domain::Service(Some(r)) = &mut e.domain
            && inner.contains(crate::access::root(r))
        {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            stmt.to_token_stream().to_string().hash(&mut h);
            *r = format!("{r}@{:x}", h.finish() & 0xffff_ffff);
        }
    }
    let speculate = scope.effects.speculate || attrs.iter().any(|a| is_psync_attr(a, "speculate"));
    Class::Node(Box::new(Node {
        effects: found.effects,
        unclassified: found.unclassified,
        panics: found.panics,
        speculate,
        independent: attrs.iter().any(|a| is_psync_attr(a, "independent")),
        stmt: stmt.clone(),
        label: label_for(&defs, stmt, &x),
        defs,
        acc,
        awaits,
        blocks: blocks && !x.awaits,
        effect,
        exits: x.exits,
        line: line_of(stmt.span()),
    }))
}
