//! Effect summaries from MIR, and the checks of `#[psync::graph]` rewrites.

use psync_check::{
    Access, CallFact, CrateFindings, CrateSummaries, Domain, Effect, Finding, FnSummary, Resolved,
    Severity, conflict, env,
};
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{DefId, LOCAL_CRATE, LocalDefId};
use rustc_middle::mir::{self, AggregateKind, Operand, Rvalue, StatementKind, TerminatorKind};
use rustc_middle::ty::{self, GenericArgs, Instance, InstanceKind, Ty, TyCtxt, TypingEnv};
use rustc_span::Span;
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

/// An effect inside one body, with the span of the call in that body that leads to it.
#[derive(Clone, Debug)]
pub struct Located {
    pub effect: Effect,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub effects: Vec<Located>,
    pub opaque: Vec<String>,
    /// May panic explicitly in the user's code.
    pub panics: bool,
    /// Trait methods reached through `dyn` or unresolved generics (keys), for the link step.
    pub deferred: Vec<String>,
}

impl Summary {
    /// Grows whenever an absorb adds anything (effects, opaque callees, panics, deferred calls).
    fn size(&self) -> (usize, usize, bool, usize) {
        (
            self.effects.len(),
            self.opaque.len(),
            self.panics,
            self.deferred.len(),
        )
    }

    /// Add a callee's summary. `srcs` says what the call passes as each parameter; without it
    /// (or for a parameter it cannot follow) an effect on a parameter is on anything.
    fn absorb(&mut self, other: &Summary, span: Span, via: Option<&str>, srcs: Option<&[Src]>) {
        for l in &other.effects {
            let mut effect = at_call(&l.effect, srcs);
            if let Some(v) = via {
                effect.via.insert(0, v.to_string());
            }
            self.add(effect, span);
        }
        self.opaque.extend(other.opaque.iter().cloned());
        self.panics |= other.panics;
        for d in &other.deferred {
            if !self.deferred.contains(d) {
                self.deferred.push(d.clone());
            }
        }
    }

    fn add(&mut self, effect: Effect, span: Span) {
        let same = |l: &Located| {
            l.effect.domain == effect.domain
                && l.effect.access == effect.access
                && l.effect.target == effect.target
                && l.effect.arg == effect.arg
        };
        if !self.effects.iter().any(same) {
            self.effects.push(Located { effect, span });
        }
    }
}

/// What a call passes as one argument, followed back through the caller's body.
#[derive(Clone, Debug, PartialEq)]
pub enum Src {
    /// The caller's own parameter, unchanged.
    Param(usize),
    /// A string constant.
    Const(String),
    Unknown,
}

/// `effect` as seen by a caller that passes `srcs`: an effect on parameter `j` becomes one on
/// what the call passes there.
fn at_call(effect: &Effect, srcs: Option<&[Src]>) -> Effect {
    let mut e = effect.clone();
    if let Some(j) = e.arg.take() {
        match srcs.and_then(|s| s.get(j)) {
            Some(Src::Param(k)) => e.arg = Some(*k),
            Some(Src::Const(c)) => e.target = Some(c.clone()),
            _ => {}
        }
    }
    e
}

/// Standard-library conversions that keep the path or string they are given
/// (`<PathBuf as Deref>::deref`, `AsRef::<Path>::as_ref`, `Path::to_path_buf`).
const SAME_VALUE: &[&str] = &[
    "as_ref",
    "deref",
    "borrow",
    "as_path",
    "as_os_str",
    "as_str",
    "to_path_buf",
    "to_owned",
    "to_string",
    "clone",
    "into",
    "from",
];

/// What each local of `body` holds, as a [`Src`]. A parameter is followed only if it is never
/// assigned or borrowed mutably, and another local only if it is assigned once, from a copy, move
/// or shared borrow of a followed local (through `*`), a string constant, or a [`SAME_VALUE`]
/// call of the standard library on one; everything else is `Unknown`.
fn sources<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &mir::Body<'tcx>,
    constant: impl Fn(&Operand<'tcx>) -> Option<String>,
) -> Vec<Src> {
    #[derive(Clone)]
    enum Def {
        Alias(mir::Local),
        Const(String),
        Other,
    }
    let n = body.local_decls.len();
    let mut defs: Vec<Vec<Def>> = vec![Vec::new(); n];
    let mut tainted = vec![false; n];
    let plain = |p: &mir::Place<'tcx>| {
        p.projection
            .iter()
            .all(|e| matches!(e, mir::ProjectionElem::Deref))
            .then_some(p.local)
    };
    let of_operand = |op: &Operand<'tcx>| match op {
        Operand::Copy(p) | Operand::Move(p) => plain(p).map_or(Def::Other, Def::Alias),
        c => constant(c).map_or(Def::Other, Def::Const),
    };
    let mut assign = |place: &mir::Place<'tcx>, def: Def, defs: &mut Vec<Vec<Def>>| {
        if place.projection.is_empty() {
            defs[place.local.as_usize()].push(def);
        } else {
            tainted[place.local.as_usize()] = true;
        }
    };
    let mut borrowed_mut = Vec::new();
    for data in body.basic_blocks.iter() {
        for stmt in &data.statements {
            match &stmt.kind {
                StatementKind::Assign(a) => {
                    let (place, rv) = &**a;
                    let def = match rv {
                        Rvalue::Use(op, _) => of_operand(op),
                        Rvalue::CopyForDeref(p) => plain(p).map_or(Def::Other, Def::Alias),
                        Rvalue::Ref(_, mir::BorrowKind::Shared | mir::BorrowKind::Fake(_), p) => {
                            plain(p).map_or(Def::Other, Def::Alias)
                        }
                        _ => Def::Other,
                    };
                    match rv {
                        Rvalue::Ref(_, mir::BorrowKind::Mut { .. }, p)
                        | Rvalue::RawPtr(mir::RawPtrKind::Mut, p) => borrowed_mut.push(p.local),
                        _ => {}
                    }
                    assign(place, def, &mut defs);
                }
                StatementKind::SetDiscriminant { place, .. } => borrowed_mut.push(place.local),
                _ => {}
            }
        }
        if let TerminatorKind::Call {
            func,
            args,
            destination,
            ..
        } = &data.terminator().kind
        {
            let same = func.const_fn_def().is_some_and(|(callee, _)| {
                matches!(
                    tcx.crate_name(callee.krate).as_str(),
                    "std" | "core" | "alloc"
                ) && tcx
                    .opt_item_name(callee)
                    .is_some_and(|n| SAME_VALUE.contains(&n.as_str()))
            });
            let def = match args.first() {
                Some(a) if same && args.len() == 1 => of_operand(&a.node),
                _ => Def::Other,
            };
            assign(destination, def, &mut defs);
        }
    }
    for l in borrowed_mut {
        tainted[l.as_usize()] = true;
    }
    fn resolve(
        l: usize,
        arg_count: usize,
        defs: &[Vec<Def>],
        tainted: &[bool],
        depth: usize,
    ) -> Src {
        if tainted[l] || depth > 64 {
            return Src::Unknown;
        }
        if (1..=arg_count).contains(&l) {
            return if defs[l].is_empty() {
                Src::Param(l - 1)
            } else {
                Src::Unknown
            };
        }
        match &defs[l][..] {
            [Def::Alias(x)] => resolve(x.as_usize(), arg_count, defs, tainted, depth + 1),
            [Def::Const(c)] => Src::Const(c.clone()),
            _ => Src::Unknown,
        }
    }
    (0..n)
        .map(|l| resolve(l, body.arg_count, &defs, &tainted, 0))
        .collect()
}

/// A call from one body to a local body: a function, or a closure or coroutine it creates.
#[derive(Clone, Debug)]
pub struct Edge {
    callee: DefId,
    /// What the call passes as each of the callee's parameters.
    srcs: Vec<Src>,
    span: Span,
    /// The callee's path, recorded in the effects' call chains (`None` for nested bodies).
    via: Option<String>,
}

/// One body's own effects (everything but its calls of local bodies), and those calls.
#[derive(Debug)]
pub struct Local {
    own: Summary,
    calls: Vec<Edge>,
}

/// The state of Tarjan's algorithm.
#[derive(Default)]
struct Tarjan {
    next: usize,
    index: HashMap<DefId, usize>,
    low: HashMap<DefId, usize>,
    stack: Vec<DefId>,
    on_stack: HashSet<DefId>,
}

/// A definition's path for people: `load` rather than `load::{closure#0}`.
fn display_path(tcx: TyCtxt<'_>, def_id: DefId) -> String {
    let path = tcx.def_path_str(def_id);
    let mut out = String::with_capacity(path.len());
    let mut rest = path.as_str();
    while let Some(at) = rest.find("::{") {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('}') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// The span of the first call in `def_id`'s body (or the closures and coroutines it creates)
/// written by the user rather than generated by the macro, to point at a chain's own statements.
fn user_span(tcx: TyCtxt<'_>, def_id: DefId) -> Option<Span> {
    if !def_id.is_local() || !tcx.is_mir_available(def_id) {
        return None;
    }
    let body = tcx.optimized_mir(def_id);
    let own = body.basic_blocks.iter().find_map(|data| {
        let t = data.terminator();
        let span = t.source_info.span;
        (matches!(t.kind, TerminatorKind::Call { .. }) && !span.from_expansion()).then_some(span)
    });
    own.or_else(|| {
        body.basic_blocks
            .iter()
            .flat_map(|d| &d.statements)
            .find_map(|stmt| {
                if let StatementKind::Assign(assign) = &stmt.kind
                    && let Rvalue::Aggregate(kind, _) = &assign.1
                    && let AggregateKind::Closure(nested, _)
                    | AggregateKind::Coroutine(nested, _)
                    | AggregateKind::CoroutineClosure(nested, _) = **kind
                {
                    user_span(tcx, nested)
                } else {
                    None
                }
            })
    })
}

/// Crates whose functions are never followed: their I/O is recognized by path instead.
const OPAQUE_STD: &[&str] = &["std", "core", "alloc", "proc_macro", "test"];
/// Async runtimes and file helpers whose I/O is recognized by path, like std's.
const IO_CRATES: &[&str] = &[
    "tokio",
    "async_std",
    "smol",
    "async_fs",
    "async_process",
    "fs_err",
];

pub struct Analyzer<'tcx> {
    tcx: TyCtxt<'tcx>,
    memo: HashMap<DefId, Rc<Summary>>,
    /// Each body's own effects and its calls of local bodies, read from MIR once.
    locals: HashMap<DefId, Rc<Local>>,
    external: Option<HashMap<String, FnSummary>>,
    /// Stable ids of crates the driver summarized.
    summarized: HashSet<String>,
    summary_dir: Option<PathBuf>,
    /// The crate being compiled comes from crates.io or git (not the user's workspace): its
    /// summaries keep only real I/O, never verbs of the APIs it calls internally.
    library: bool,
    /// Deferred trait methods resolved over the whole build by a previous run's link step.
    linked: BTreeMap<String, Resolved>,
}

impl<'tcx> Analyzer<'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>) -> Self {
        Self {
            tcx,
            memo: HashMap::new(),
            locals: HashMap::new(),
            external: None,
            summarized: HashSet::new(),
            summary_dir: std::env::var_os(env::SUMMARIES).map(PathBuf::from),
            library: is_registry(tcx, rustc_hir::def_id::CRATE_DEF_ID.to_def_id()),
            linked: std::env::var_os(env::LINKED)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
        }
    }

    fn has_body(&self, def_id: DefId) -> bool {
        def_id.is_local()
            && matches!(
                self.tcx.def_kind(def_id),
                DefKind::Fn | DefKind::AssocFn | DefKind::Closure
            )
            && self.tcx.is_mir_available(def_id)
    }

    /// The effects of a local function, closure or coroutine, including the closures and
    /// coroutines it creates (an `async fn`'s body is a coroutine it returns), through every
    /// local callee.
    ///
    /// A summary is the least fixed point of `S(f) = own(f) ∪ ⋃ S(g)` over the local calls
    /// `f → g`. Recursive functions make the call graph cyclic, so the bodies are grouped into
    /// strongly connected components (Tarjan's algorithm, callees before callers), and each
    /// component is iterated until its summaries stop growing. Every member of a cycle then gets
    /// the effects of the whole cycle, whichever member is asked for first.
    pub fn summarize(&mut self, def_id: DefId) -> Rc<Summary> {
        if let Some(s) = self.memo.get(&def_id) {
            return s.clone();
        }
        if !self.has_body(def_id) {
            return Rc::new(Summary::default());
        }
        let mut walk = Tarjan::default();
        self.component(def_id, &mut walk);
        self.memo[&def_id].clone()
    }

    /// Tarjan's strongly connected components over local calls, from `v`. When `v` closes a
    /// component, its members' summaries are computed together and memoized.
    fn component(&mut self, v: DefId, t: &mut Tarjan) {
        let index = t.next;
        t.next += 1;
        t.index.insert(v, index);
        t.low.insert(v, index);
        t.stack.push(v);
        t.on_stack.insert(v);
        let local = self.local(v);
        for edge in &local.calls {
            let w = edge.callee;
            if self.memo.contains_key(&w) || !self.has_body(w) {
                continue;
            }
            if !t.index.contains_key(&w) {
                self.component(w, t);
                let low = t.low[&v].min(t.low[&w]);
                t.low.insert(v, low);
            } else if t.on_stack.contains(&w) {
                let low = t.low[&v].min(t.index[&w]);
                t.low.insert(v, low);
            }
        }
        if t.low[&v] != index {
            return;
        }
        // `v` is the root of a component: its members are on the stack above it.
        let mut members = Vec::new();
        while let Some(w) = t.stack.pop() {
            t.on_stack.remove(&w);
            members.push(w);
            if w == v {
                break;
            }
        }
        let locals: Vec<Rc<Local>> = members.iter().map(|m| self.local(*m)).collect();
        let mut current: HashMap<DefId, Summary> = members
            .iter()
            .zip(&locals)
            .map(|(m, l)| (*m, l.own.clone()))
            .collect();
        // Summaries only grow, and there are finitely many distinct effects: this terminates.
        // A component without a cycle (one member, not calling itself) takes one round.
        loop {
            let mut changed = false;
            for (m, local) in members.iter().zip(&locals) {
                for edge in &local.calls {
                    let callee = match self.memo.get(&edge.callee) {
                        Some(done) => (**done).clone(),
                        None => current.get(&edge.callee).cloned().unwrap_or_default(),
                    };
                    let summary = current.get_mut(m).expect("member");
                    let before = summary.size();
                    summary.absorb(&callee, edge.span, edge.via.as_deref(), Some(&edge.srcs));
                    summary.opaque.sort();
                    summary.opaque.dedup();
                    changed |= summary.size() != before;
                }
            }
            if !changed {
                break;
            }
        }
        for (m, summary) in current {
            self.memo.insert(m, Rc::new(summary));
        }
    }

    /// What one body does by itself, and which local bodies it calls, read from its MIR.
    fn local(&mut self, def_id: DefId) -> Rc<Local> {
        if let Some(l) = self.locals.get(&def_id) {
            return l.clone();
        }
        let tcx = self.tcx;
        let body = tcx.optimized_mir(def_id);
        let typing_env = TypingEnv::post_analysis(tcx, def_id);
        let mut own = Summary::default();
        let mut calls = Vec::new();
        let held = sources(tcx, body, |op| self.string_constant(op));
        for data in body.basic_blocks.iter() {
            for stmt in &data.statements {
                if let StatementKind::Assign(assign) = &stmt.kind
                    && let Rvalue::Aggregate(kind, _) = &assign.1
                    && let AggregateKind::Closure(nested, _)
                    | AggregateKind::Coroutine(nested, _)
                    | AggregateKind::CoroutineClosure(nested, _) = **kind
                {
                    calls.push(Edge {
                        callee: nested,
                        srcs: Vec::new(),
                        span: stmt.source_info.span,
                        via: None,
                    });
                }
            }
            let terminator = data.terminator();
            if let TerminatorKind::Call { func, args, .. } = &terminator.kind {
                let span = terminator.source_info.span;
                match func.const_fn_def() {
                    Some((callee, gargs)) => {
                        let args: Vec<&Operand<'tcx>> = args.iter().map(|a| &a.node).collect();
                        let srcs: Vec<Src> = args
                            .iter()
                            .map(|a| match a {
                                Operand::Copy(p) | Operand::Move(p)
                                    if p.projection
                                        .iter()
                                        .all(|e| matches!(e, mir::ProjectionElem::Deref)) =>
                                {
                                    held[p.local.as_usize()].clone()
                                }
                                c => self.string_constant(c).map_or(Src::Unknown, Src::Const),
                            })
                            .collect();
                        self.call(
                            &mut own,
                            Some(&mut calls),
                            typing_env,
                            callee,
                            gargs,
                            &args,
                            &srcs,
                            span,
                        );
                    }
                    None => own.opaque.push("call through a function pointer".into()),
                }
            }
        }
        own.opaque.sort();
        own.opaque.dedup();
        let local = Rc::new(Local { own, calls });
        self.locals.insert(def_id, local.clone());
        local
    }

    /// Add the effects of calling `callee` to `summary`. With `calls`, a call of a local body is
    /// recorded there instead of followed (for [`Self::component`]).
    #[allow(clippy::too_many_arguments)]
    fn call(
        &mut self,
        summary: &mut Summary,
        calls: Option<&mut Vec<Edge>>,
        typing_env: TypingEnv<'tcx>,
        callee: DefId,
        gargs: GenericArgs<'tcx>,
        args: &[&Operand<'tcx>],
        srcs: &[Src],
        span: Span,
    ) {
        let tcx = self.tcx;
        let instance = Instance::try_resolve(tcx, typing_env, callee, gargs)
            .ok()
            .flatten();
        let resolved = instance.map(|i| i.def_id()).unwrap_or(callee);
        // A trait method called through `dyn`, or on a generic the compiler cannot resolve here:
        // its implementations may be anywhere in the build, so the link step resolves it.
        let unresolved = match instance {
            Some(i) => matches!(i.def, InstanceKind::Virtual(..)),
            None => true,
        };
        if unresolved
            && let Some(trait_id) = tcx.trait_of_assoc(callee)
            && !self.library
            && !is_registry(tcx, trait_id)
            && !OPAQUE_STD.contains(&tcx.crate_name(trait_id.krate).as_str())
        {
            let key = format!("{:?}", tcx.def_path_hash(callee));
            let path = display_path(tcx, callee);
            if let Some(r) = self.linked.get(&key) {
                for e in &r.effects {
                    let mut e = at_call(e, None);
                    e.via.insert(0, path.clone());
                    summary.add(e, span);
                }
                summary.panics |= r.panics;
            }
            if !summary.deferred.contains(&key) {
                summary.deferred.push(key);
            }
            return;
        }
        // Through `dyn` or a generic, a trait of another published crate (`AsyncWrite`,
        // `tower::Service`) may be implemented by anything: unknown, not "no effects".
        if unresolved
            && !self.library
            && let Some(trait_id) = tcx.trait_of_assoc(callee)
            && !OPAQUE_STD.contains(&tcx.crate_name(trait_id.krate).as_str())
        {
            summary
                .opaque
                .push(format!("dyn {}", display_path(tcx, callee)));
        }
        let path = display_path(tcx, resolved);
        let krate = tcx.crate_name(resolved.krate);
        let krate = krate.as_str();
        let name = tcx
            .opt_item_name(resolved)
            .map(|n| n.to_string())
            .unwrap_or_default();
        let target = args.first().and_then(|a| self.string_constant(a));
        // The target of an operation on the call's argument `j`: a constant, or the caller's
        // parameter it holds.
        let on = |j: usize| match srcs.get(j) {
            Some(Src::Const(c)) => (Some(c.clone()), None),
            Some(Src::Param(k)) => (None, Some(*k)),
            _ => (None, None),
        };
        let mut add = |domain, access, (target, arg): (Option<String>, Option<usize>)| {
            summary.add(
                Effect {
                    domain,
                    access,
                    target,
                    via: vec![path.clone()],
                    arg,
                },
                span,
            )
        };
        let library = self.library;
        // psync's own runtime helpers are the generated code's plumbing.
        if krate == "psync" {
            return;
        }
        // Explicit panics in the user's code are exits too: a write must not run before them.
        if !library && panicking(krate, &path, &name) {
            summary.panics = true;
            return;
        }
        if OPAQUE_STD.contains(&krate) || IO_CRATES.contains(&krate) {
            // Generic I/O helpers (`AsyncWriteExt::shutdown`, `Write::write_all`) act on whatever
            // writer they are given: the receiver's type is the resource.
            let self_ty = tcx
                .trait_of_assoc(callee)
                .and_then(|_| gargs.types().next())
                .map(|t| format!("{t}"));
            if !library
                && let Some((domain, access)) = io_trait_effect(&path, &name, self_ty.as_deref())
            {
                add(domain, access, (None, None));
                return;
            }
            if let Some((domain, access, keeps_target)) = io_effect(&path, &name)
                // Inside libraries, socket traffic is transport, not meaning: every request
                // writes to a socket.
                && !(library && domain == Domain::Net)
            {
                // `copy(from, to)`, `rename(from, to)`, `hard_link(original, link)`,
                // `symlink(original, link)`: the second path is written too. `copy` only reads
                // its source.
                let second = (domain == Domain::Fs && keeps_target)
                    .then(|| two_path_fs(&name))
                    .flatten();
                match second {
                    Some(first_access) => {
                        add(Domain::Fs, first_access, on(0));
                        add(Domain::Fs, Access::Write, on(1));
                    }
                    None => add(
                        domain,
                        access,
                        if keeps_target { on(0) } else { (None, None) },
                    ),
                }
            }
            return;
        }
        if resolved.is_local() {
            match calls {
                Some(calls) if self.has_body(resolved) => calls.push(Edge {
                    callee: resolved,
                    srcs: srcs.to_vec(),
                    span,
                    via: Some(path.clone()),
                }),
                _ => {
                    let inner = self.summarize(resolved);
                    summary.absorb(&inner, span, Some(&path), Some(srcs));
                }
            }
            if !self.has_body(resolved) && !library {
                self.by_name(summary, &path, &name, target, span);
            }
            return;
        }
        // Another crate. Its summary says what its body does; at the boundary between the user's
        // code and a published API, the API's verb says what the call means.
        let key = format!("{:?}", tcx.def_path_hash(resolved));
        let external = self.external().get(&key).cloned();
        let use_verb = !library
            && is_registry(tcx, resolved)
            && returns_future(tcx, resolved)
            && psync_graph::verb_of(&name).is_some()
            && !is_plumbing(&name);
        let summarized = self
            .summarized
            .contains(&format!("{:?}", tcx.stable_crate_id(resolved.krate)));
        match external {
            // Summarized, without effects.
            None if summarized && !use_verb => {}
            Some(s) if !use_verb => {
                for effect in s.effects {
                    let mut effect = at_call(&effect, Some(srcs));
                    effect.via.insert(0, path.clone());
                    summary.add(effect, span);
                }
                summary.opaque.extend(s.opaque);
                summary.panics |= s.panics;
                for d in s.deferred {
                    if !summary.deferred.contains(&d) {
                        summary.deferred.push(d);
                    }
                }
            }
            _ if library || !returns_future(tcx, resolved) => {}
            _ => self.by_name(summary, &path, &name, target, span),
        }
    }

    /// Classify a call into another crate's API by the verb its name starts with.
    fn by_name(
        &self,
        summary: &mut Summary,
        path: &str,
        name: &str,
        target: Option<String>,
        span: Span,
    ) {
        let verb = if is_plumbing(name) {
            None
        } else {
            psync_graph::verb_of(name)
        };
        let Some(verb) = verb else {
            // Functions without a verb in crates without a summary (proc macros, build-only
            // crates) are usually helpers; record them for `cargo psync summaries`.
            if !name.is_empty() && !is_plumbing(name) {
                summary.opaque.push(path.to_string());
            }
            return;
        };
        let access = match verb {
            psync_graph::Verb::Read => Access::Read,
            psync_graph::Verb::Write => Access::Write,
            psync_graph::Verb::Ordered => Access::Ordered,
        };
        // The domain is what the function belongs to: `kube::Api::<K>` for `kube::Api::<K>::list`.
        let domain = path.rsplit_once("::").map_or(path, |(owner, _)| owner);
        summary.add(
            Effect {
                domain: Domain::Service(domain.to_string()),
                access,
                target,
                via: vec![path.to_string()],
                arg: None,
            },
            span,
        );
    }

    fn external(&mut self) -> &HashMap<String, FnSummary> {
        if self.external.is_none() {
            let mut map = HashMap::new();
            if let Some(dir) = &self.summary_dir {
                for krate in psync_check::read_all::<CrateSummaries>(dir) {
                    self.summarized.insert(krate.crate_id.clone());
                    map.extend(krate.functions);
                }
            }
            self.external = Some(map);
        }
        self.external.as_ref().expect("loaded")
    }

    /// The value of a `&str` constant operand.
    fn string_constant(&self, op: &Operand<'tcx>) -> Option<String> {
        let Operand::Constant(c) = op else {
            return None;
        };
        let mir::Const::Val(value, ty) = c.const_ else {
            return None;
        };
        if !matches!(ty.kind(), ty::Ref(_, inner, _) if inner.is_str()) {
            return None;
        }
        let bytes = value.try_get_slice_bytes_for_diagnostics(self.tcx)?;
        String::from_utf8(bytes.to_vec()).ok()
    }

    /// The functions `#[psync::graph]` annotated: each contains the item `__PSYNC_GRAPH`.
    fn annotated(&self) -> Vec<DefId> {
        let tcx = self.tcx;
        tcx.hir_body_owners()
            .filter(|o| {
                tcx.def_kind(*o) == DefKind::Const
                    && tcx
                        .opt_item_name(o.to_def_id())
                        .is_some_and(|n| n.as_str() == "__PSYNC_GRAPH")
            })
            .map(|o| tcx.parent(o.to_def_id()))
            .collect()
    }

    /// Facts about every call in the annotated functions (and the closures and async blocks in
    /// them), keyed by the source location of the callee's name, for the macro's next expansion.
    fn call_facts(&mut self) -> BTreeMap<String, CallFact> {
        let tcx = self.tcx;
        let mut out = BTreeMap::new();
        let mut bodies = self.annotated();
        let mut seen = HashSet::new();
        while let Some(def_id) = bodies.pop() {
            if !seen.insert(def_id) || !self.has_body(def_id) {
                continue;
            }
            let body = tcx.optimized_mir(def_id);
            let typing_env = TypingEnv::post_analysis(tcx, def_id);
            for data in body.basic_blocks.iter() {
                for stmt in &data.statements {
                    if let StatementKind::Assign(assign) = &stmt.kind
                        && let Rvalue::Aggregate(kind, _) = &assign.1
                        && let AggregateKind::Closure(nested, _)
                        | AggregateKind::Coroutine(nested, _)
                        | AggregateKind::CoroutineClosure(nested, _) = **kind
                    {
                        bodies.push(nested);
                    }
                }
                let terminator = data.terminator();
                let TerminatorKind::Call {
                    func,
                    args,
                    fn_span,
                    ..
                } = &terminator.kind
                else {
                    continue;
                };
                let Some((callee, gargs)) = func.const_fn_def() else {
                    continue;
                };
                if fn_span.from_expansion() || tcx.crate_name(callee.krate).as_str() == "psync" {
                    continue;
                }
                let Some(site) = site_key(tcx, *fn_span) else {
                    continue;
                };
                let args: Vec<&Operand<'tcx>> = args.iter().map(|a| &a.node).collect();
                let mut one = Summary::default();
                // In terms of the call's own arguments: the macro reads them at the call site.
                let srcs: Vec<Src> = (0..args.len()).map(Src::Param).collect();
                self.call(
                    &mut one, None, typing_env, callee, gargs, &args, &srcs, *fn_span,
                );
                let name = tcx
                    .opt_item_name(callee)
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                out.insert(
                    site,
                    CallFact {
                        callee: display_path(tcx, callee),
                        name,
                        effects: one.effects.into_iter().map(|l| l.effect).collect(),
                        panics: one.panics,
                        deferred: one.deferred,
                        opaque: !one.opaque.is_empty(),
                    },
                );
            }
        }
        out
    }

    /// Write the summaries of every local function for crates that depend on this one.
    pub fn write_summaries(&mut self) {
        let Some(dir) = self.summary_dir.clone() else {
            return;
        };
        let tcx = self.tcx;
        let mut out = CrateSummaries {
            crate_name: tcx.crate_name(LOCAL_CRATE).to_string(),
            crate_id: format!("{:?}", tcx.stable_crate_id(LOCAL_CRATE)),
            ..Default::default()
        };
        let owners: Vec<LocalDefId> = tcx.hir_body_owners().collect();
        for owner in owners {
            let def_id = owner.to_def_id();
            if !matches!(tcx.def_kind(def_id), DefKind::Fn | DefKind::AssocFn) {
                continue;
            }
            let key = format!("{:?}", tcx.def_path_hash(def_id));
            // Methods implementing a trait method, for the link step's resolution of `dyn` calls.
            if tcx.def_kind(def_id) == DefKind::AssocFn
                && let Some(trait_item) = tcx.associated_item(def_id).trait_item_def_id()
                && !OPAQUE_STD.contains(&tcx.crate_name(trait_item.krate).as_str())
            {
                out.impls
                    .entry(format!("{:?}", tcx.def_path_hash(trait_item)))
                    .or_default()
                    .push(key.clone());
            }
            let s = self.summarize(def_id);
            if s.effects.is_empty() && s.opaque.is_empty() && !s.panics && s.deferred.is_empty() {
                continue;
            }
            out.functions.insert(
                key,
                FnSummary {
                    path: display_path(tcx, def_id),
                    effects: s.effects.iter().map(|l| l.effect.clone()).collect(),
                    opaque: s.opaque.clone(),
                    panics: s.panics,
                    deferred: s.deferred.clone(),
                },
            );
        }
        out.calls = self.call_facts();
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(format!(
            "{}-{:?}.json",
            out.crate_name,
            tcx.stable_crate_id(LOCAL_CRATE)
        ));
        if let Ok(json) = serde_json::to_string(&out) {
            let _ = std::fs::write(file, json);
        }
    }
}

/// Insert `#[psync::seq]` before the statement containing `span` (or, for a loop prefix, before
/// the `for` loop around it), on its own line with the statement's indentation.
fn seq_fix(tcx: TyCtxt<'_>, span: Span, enclosing_loop: bool) -> Option<psync_check::Fix> {
    let sm = tcx.sess.source_map();
    let pos = sm.lookup_byte_offset(span.lo());
    let src = pos.sf.src.as_ref()?;
    let rustc_span::FileName::Real(real) = &pos.sf.name else {
        return None;
    };
    let path = real.local_path()?;
    let file = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let bytes = src.as_bytes();
    let mut at = pos.pos.0 as usize;
    // Walk back to the start of the statement: the previous `;`, `{` or `}` outside brackets.
    let mut depth = 0i32;
    let start = loop {
        if at == 0 {
            break 0;
        }
        at -= 1;
        match bytes[at] {
            b')' | b']' => depth += 1,
            b'(' | b'[' => depth -= 1,
            b'}' if depth == 0 => break at + 1,
            b'{' | b';' if depth == 0 => break at + 1,
            b'}' => depth += 1,
            b'{' => depth -= 1,
            _ => {}
        }
    };
    let mut start = start;
    // For a loop, the statement found is inside the body: continue to the `for` that opens it.
    if enclosing_loop {
        let before = &src[..start];
        let header = before.rfind("for ")?;
        start = before[..header].rfind('\n').map_or(0, |i| i + 1);
    }
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = src[line_start..start]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    Some(psync_check::Fix {
        file: file.display().to_string(),
        offset: start,
        insert: format!("#[psync::seq]\n{indent}"),
    })
}

/// `/abs/path/file.rs:line:column` of a span's start (column counted in characters, from 1), the
/// key the macro computes from its tokens' spans.
fn site_key(tcx: TyCtxt<'_>, span: Span) -> Option<String> {
    let loc = tcx.sess.source_map().lookup_char_pos(span.lo());
    let rustc_span::FileName::Real(real) = &loc.file.name else {
        return None;
    };
    let path = real.local_path()?;
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Some(format!("{}:{}:{}", path.display(), loc.line, loc.col.0 + 1))
}

/// Calls that panic on purpose: `panic!`, `assert!`, `unreachable!`, `todo!`, `unwrap`, `expect`.
fn panicking(krate: &str, path: &str, name: &str) -> bool {
    if !OPAQUE_STD.contains(&krate) {
        return false;
    }
    path.contains("::panicking::")
        || matches!(
            name,
            "begin_panic" | "panic_fmt" | "panic_display" | "assert_failed"
        )
        || ((path.contains("Option") || path.contains("Result"))
            && matches!(name, "unwrap" | "expect" | "unwrap_err" | "expect_err"))
}

/// Whether calling `def_id` returns a future: an `async fn`, or a function or method returning
/// `impl Future`, `Pin<Box<dyn Future>>` or a named future type. Like the macro, the driver reads
/// verbs only on awaited calls: `HashMap::insert` and `Serializer::append_pair` are not I/O.
fn returns_future(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    if !matches!(tcx.def_kind(def_id), DefKind::Fn | DefKind::AssocFn) {
        return false;
    }
    if tcx.asyncness(def_id).is_async() {
        return true;
    }
    let output = tcx.fn_sig(def_id).skip_binder().output().skip_binder();
    let shown = format!("{output:?}");
    shown.contains("Future") || shown.contains("Fut") || {
        // `impl Future` and named futures: ask whether the type implements `Future`.
        let future = tcx.lang_items().future_trait();
        future.is_some_and(|trait_id| {
            tcx.explicit_item_bounds(match output.kind() {
                ty::Alias(_, alias) => match alias.kind {
                    ty::AliasTyKind::Opaque { def_id } => def_id,
                    _ => return false,
                },
                _ => return false,
            })
            .skip_binder()
            .iter()
            .any(|(clause, _)| {
                clause
                    .as_trait_clause()
                    .is_some_and(|t| t.def_id() == trait_id)
            })
        })
    }
}

/// Whether `def_id` is defined in a crate from crates.io or a git dependency, rather than in the
/// user's workspace or a path dependency.
fn is_registry(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    let file = tcx.sess.source_map().span_to_filename(tcx.def_span(def_id));
    let name = format!("{file:?}");
    name.contains("/.cargo/registry/") || name.contains("/.cargo/git/")
}

/// Glue that is neither I/O nor an API call: conversions, builders, iterators, formatting.
fn is_plumbing(name: &str) -> bool {
    matches!(
        name,
        "new"
            | "from"
            | "into"
            | "default"
            | "clone"
            | "build"
            | "with"
            | "map"
            | "and_then"
            | "unwrap"
            | "expect"
            | "as_ref"
            | "as_mut"
            | "deref"
            | "deref_mut"
            | "drop"
            | "fmt"
            | "eq"
            | "ne"
            | "cmp"
            | "partial_cmp"
            | "hash"
            | "poll"
            | "into_future"
            | "call"
            | "call_mut"
            | "call_once"
            | "branch"
            | "from_residual"
            | "next"
            | "into_iter"
    ) || name.starts_with("with_")
        || name.starts_with("to_")
        || name.starts_with("as_")
        || name.starts_with("into_")
        || name.starts_with("from_")
}

/// Methods of the I/O traits (`AsyncWrite`/`AsyncWriteExt`, `io::Write`, `AsyncRead`, `io::Read`)
/// on a writer or reader that is not an in-memory buffer: a write or read of that object, named
/// by its type. Files are classified by [`io_effect`].
fn io_trait_effect(path: &str, name: &str, self_ty: Option<&str>) -> Option<(Domain, Access)> {
    let ty = self_ty?;
    let in_memory = [
        "Vec<",
        "Cursor<",
        "String",
        "Sink",
        "Empty",
        "&mut [u8]",
        "[u8]",
        "Repeat",
    ]
    .iter()
    .any(|m| ty.contains(m));
    if in_memory || ty.contains("fs::File") {
        return None;
    }
    let writes = (path.contains("AsyncWrite") || path.contains("io::Write"))
        && [
            "write",
            "flush",
            "shutdown",
            "poll_write",
            "poll_flush",
            "poll_shutdown",
        ]
        .iter()
        .any(|p| name.starts_with(p));
    let reads =
        (path.contains("AsyncRead") || path.contains("io::Read") || path.contains("BufRead"))
            && (name.starts_with("read")
                || name.starts_with("poll_read")
                || name.starts_with("fill_buf"));
    let domain = || Domain::Service(format!("io {ty}"));
    match (writes, reads) {
        (true, _) => Some((domain(), Access::Write)),
        (false, true) => Some((domain(), Access::Read)),
        _ => None,
    }
}

/// File operations on two paths, which write the second: the access to the first.
fn two_path_fs(name: &str) -> Option<Access> {
    match name {
        "copy" => Some(Access::Read),
        "rename" | "hard_link" | "symlink" | "soft_link" | "symlink_file" | "symlink_dir" => {
            Some(Access::Write)
        }
        _ => None,
    }
}

/// File, socket, process, environment and output operations of std and the async runtimes,
/// recognized by path: `(domain, access, whether the first argument is the target)`.
fn io_effect(path: &str, name: &str) -> Option<(Domain, Access, bool)> {
    // File operations: free functions of an `fs` module, and methods of open files, directory
    // builders and directory listings. `Metadata`, `Permissions` and `FileType` are plain values.
    let segments: Vec<&str> = path.split("::").collect();
    let free_fs_fn = segments.len() >= 2 && segments[segments.len() - 2] == "fs";
    let handle = ["File", "OpenOptions", "DirBuilder", "ReadDir", "DirEntry"]
        .iter()
        .any(|t| path.contains(&format!("fs::{t}")));
    if free_fs_fn || handle {
        if matches!(
            name,
            "new"
                | "options"
                | "file_name"
                | "path"
                | "file_type"
                | "try_clone"
                | "lock"
                | "unlock"
                | "try_lock"
        ) {
            return None;
        }
        // Moving the cursor changes no file content.
        if name.starts_with("seek") || matches!(name, "rewind" | "stream_position") {
            return Some((Domain::Fs, Access::Read, false));
        }
        let reads = psync_graph::fs_reads(name)
            || name.starts_with("read")
            || name.starts_with("metadata")
            || name == "exists"
            || name == "try_exists"
            || name == "next"
            || name == "next_entry";
        let access = if path.contains("OpenOptions") && name == "open" {
            // Unknown flags: may create or truncate.
            Access::Write
        } else if reads {
            Access::Read
        } else {
            Access::Write
        };
        // Free functions and constructors name the file in their first argument; methods of an
        // open file act on that file, whatever their argument.
        let named = free_fs_fn || matches!(name, "open" | "create" | "create_new");
        return Some((Domain::Fs, access, named));
    }
    if path.contains("::net::") {
        let writes = name.starts_with("send") || name.starts_with("write") || name == "shutdown";
        let reads = name.starts_with("recv")
            || name.starts_with("read")
            || name.starts_with("peek")
            || name.starts_with("connect")
            || name.starts_with("accept")
            || name == "bind";
        return match (writes, reads) {
            (true, _) => Some((Domain::Net, Access::Write, false)),
            (false, true) => Some((Domain::Net, Access::Read, false)),
            _ => None,
        };
    }
    if path.contains("process::Command") || path.contains("process::Child") {
        return matches!(name, "output" | "status" | "spawn" | "kill" | "wait").then_some((
            Domain::Process,
            Access::Write,
            false,
        ));
    }
    if path.contains("process::exit") || path.contains("process::abort") {
        return Some((Domain::Process, Access::Write, false));
    }
    if path.contains("::env::") {
        return match name {
            "set_var" | "remove_var" | "set_current_dir" => {
                Some((Domain::Env, Access::Write, true))
            }
            "var" | "var_os" | "vars" | "vars_os" | "current_dir" => {
                Some((Domain::Env, Access::Read, true))
            }
            _ => None,
        };
    }
    if path.ends_with("io::_print") || path.ends_with("io::_eprint") {
        return Some((Domain::Output, Access::Ordered, false));
    }
    None
}

/// One chain of a concurrent level, as the macro tagged it.
struct Chain {
    body: DefId,
    order: u64,
    exits: bool,
    spec: bool,
    indep: bool,
    /// A job of synchronous code (`job_at`), not a future: it cannot be dropped halfway.
    sync: bool,
    span: Span,
}

/// A prefetched loop prefix, as the macro tagged it.
struct Prefix {
    body: DefId,
    terminates: bool,
    spec: bool,
}

pub struct Checker<'a, 'tcx> {
    pub analyzer: &'a mut Analyzer<'tcx>,
    pub findings: CrateFindings,
    notes: bool,
    deny: bool,
}

impl<'a, 'tcx> Checker<'a, 'tcx> {
    pub fn new(analyzer: &'a mut Analyzer<'tcx>) -> Self {
        let tcx = analyzer.tcx;
        Self {
            analyzer,
            findings: CrateFindings {
                crate_name: tcx.crate_name(LOCAL_CRATE).to_string(),
                ..Default::default()
            },
            notes: std::env::var(env::NOTES).is_ok_and(|v| v == "1"),
            deny: std::env::var(env::DENY).is_ok_and(|v| v == "1"),
        }
    }

    /// Check every rewritten function of the crate.
    pub fn run(&mut self) {
        let tcx = self.analyzer.tcx;
        let owners: Vec<LocalDefId> = tcx.hir_body_owners().collect();
        for owner in owners {
            let def_id = owner.to_def_id();
            if !self.analyzer.has_body(def_id) {
                continue;
            }
            let (levels, prefixes) = self.markers(def_id);
            if !levels.is_empty() || !prefixes.is_empty() {
                let entry = self
                    .findings
                    .functions
                    .entry(display_path(tcx, def_id))
                    .or_default();
                entry.0 += levels.len();
                entry.1 += prefixes.len();
            }
            for (_, chains) in levels {
                self.findings.checked_levels += 1;
                self.check_level(def_id, &chains);
            }
            for prefix in prefixes {
                self.findings.checked_loops += 1;
                self.check_prefix(def_id, &prefix);
            }
        }
    }

    /// The tagged levels and loop prefixes in one body.
    fn markers(&self, def_id: DefId) -> (Vec<(u64, Vec<Chain>)>, Vec<Prefix>) {
        let tcx = self.analyzer.tcx;
        let body = tcx.optimized_mir(def_id);
        let mut levels: Vec<(u64, Vec<Chain>)> = Vec::new();
        let mut prefixes = Vec::new();
        for data in body.basic_blocks.iter() {
            let terminator = data.terminator();
            let TerminatorKind::Call { func, .. } = &terminator.kind else {
                continue;
            };
            let Some((callee, gargs)) = func.const_fn_def() else {
                continue;
            };
            if tcx.crate_name(callee.krate).as_str() != "psync" {
                continue;
            }
            let name = tcx.item_name(callee);
            let span = terminator.source_info.span;
            let value = |i: usize| match gargs.const_at(i).kind() {
                ty::ConstKind::Value(v) => Some(v),
                _ => None,
            };
            let usize_at = |i: usize| value(i)?.try_to_target_usize(tcx);
            let bool_at = |i: usize| value(i)?.try_to_bool();
            match name.as_str() {
                // A future of an async level, or a job of a synchronous one.
                "maybe_done_at" | "job_at" => {
                    let (Some(level), Some(order), Some(exits), Some(spec), Some(indep)) =
                        (usize_at(0), usize_at(1), bool_at(2), bool_at(3), bool_at(4))
                    else {
                        continue;
                    };
                    let Some(body) = coroutine_of(gargs.type_at(5)) else {
                        continue;
                    };
                    let chain = Chain {
                        body,
                        order,
                        exits,
                        spec,
                        indep,
                        sync: name.as_str() == "job_at",
                        span,
                    };
                    match levels.iter_mut().find(|(l, _)| *l == level) {
                        Some((_, chains)) => chains.push(chain),
                        None => levels.push((level, vec![chain])),
                    }
                }
                "over_at" => {
                    let (Some(terminates), Some(spec)) = (bool_at(1), bool_at(2)) else {
                        continue;
                    };
                    let Some(body) = coroutine_of(gargs.type_at(5)) else {
                        continue;
                    };
                    prefixes.push(Prefix {
                        body,
                        terminates,
                        spec,
                    });
                }
                _ => {}
            }
        }
        for (_, chains) in &mut levels {
            chains.sort_by_key(|c| c.order);
        }
        (levels, prefixes)
    }

    fn check_level(&mut self, function: DefId, chains: &[Chain]) {
        let summaries: Vec<Rc<Summary>> = chains
            .iter()
            .map(|c| self.analyzer.summarize(c.body))
            .collect();
        for (j, later) in chains.iter().enumerate() {
            for (i, earlier) in chains[..j].iter().enumerate() {
                // Two chains touch the same resource, at least one writing.
                if !(earlier.indep || later.indep) {
                    let pair = summaries[i].effects.iter().find_map(|a| {
                        summaries[j]
                            .effects
                            .iter()
                            .find(|b| conflict(&a.effect, &b.effect))
                            .map(|b| (a, b))
                    });
                    if let Some((a, b)) = pair {
                        let output = a.effect.domain == Domain::Output;
                        self.report(
                            if output {
                                Severity::Note
                            } else {
                                Severity::Warning
                            },
                            if output { "output-order" } else { "conflict" },
                            function,
                            b.span,
                            format!(
                                "psync runs this concurrently with other work that has a \
                                 conflicting effect: this does `{}` (via {}), the other does `{}` \
                                 (via {})",
                                b.effect.describe(),
                                b.effect.path(),
                                a.effect.describe(),
                                a.effect.path()
                            ),
                            Some((a.span, "the conflicting effect".to_string())),
                        );
                    }
                }
                // A write runs before an earlier statement that may exit is known not to. A
                // panic in the earlier chain's callees (`unwrap`, `expect`, `panic!` in the user's
                // code) is an exit too, although the macro's tokens do not show it.
                let panics = !earlier.exits && summaries[i].panics;
                if (earlier.exits || panics)
                    && !later.spec
                    && let Some(w) = summaries[j]
                        .effects
                        .iter()
                        .find(|l| l.effect.writes() && l.effect.domain != Domain::Output)
                {
                    let how = if panics { "may panic" } else { "exits early" };
                    self.report(
                        Severity::Warning,
                        "speculative-write",
                        function,
                        w.span,
                        format!(
                            "psync starts this before knowing whether an earlier statement {how}, \
                             and it does `{}` (via {}); sequential code would not have done it \
                             after that exit. Mark it `#[psync::speculate]` if that is harmless, \
                             or `#[psync::seq]`",
                            w.effect.describe(),
                            w.effect.path()
                        ),
                        Some((
                            user_span(self.analyzer.tcx, earlier.body).unwrap_or(earlier.span),
                            "the earlier work that may exit".to_string(),
                        )),
                    );
                }
            }
        }
        self.check_interior(function, chains);
        // Cancellation: dropping the function's future mid-join interrupts every chain at once.
        let writers: Vec<usize> = summaries
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.effects
                    .iter()
                    .any(|l| l.effect.writes() && l.effect.domain != Domain::Output)
            })
            .map(|(k, _)| k)
            .collect();
        // A synchronous function is not cancelled: its jobs run to the end.
        if writers.len() >= 2 && !chains.iter().all(|c| c.sync) {
            let tcx = self.analyzer.tcx;
            let span = |k: usize| user_span(tcx, chains[k].body).unwrap_or(chains[k].span);
            self.report(
                Severity::Note,
                "cancellation",
                function,
                span(writers[1]),
                format!(
                    "psync runs {} chains that write concurrently: if this function's future is \
                     dropped (a timeout, `select!`), each may stop halfway, where sequential code \
                     had at most one write in progress",
                    writers.len()
                ),
                Some((span(writers[0]), "another chain that writes".to_string())),
            );
        }
    }

    fn check_prefix(&mut self, function: DefId, prefix: &Prefix) {
        if !prefix.terminates || prefix.spec {
            return;
        }
        let summary = self.analyzer.summarize(prefix.body);
        if let Some(w) = summary
            .effects
            .iter()
            .find(|l| l.effect.writes() && l.effect.domain != Domain::Output)
        {
            let tcx = self.analyzer.tcx;
            let primary = if w.span.from_expansion() {
                user_span(tcx, prefix.body).unwrap_or(w.span)
            } else {
                w.span
            };
            self.report(
                Severity::Warning,
                "loop-speculative-write",
                function,
                primary,
                format!(
                    "psync runs this loop's prefix for every item before the loop body runs, \
                     and the body can end the loop early, but the prefix does `{}` (via {}). \
                     Sequential code would not do it for items after the exit. Mark the loop \
                     `#[psync::speculate]` if that is harmless",
                    w.effect.describe(),
                    w.effect.path()
                ),
                None,
            );
        }
    }

    /// Two chains that share a `Cell`, `RefCell`, `Mutex` or atomic interleave their updates.
    fn check_interior(&mut self, function: DefId, chains: &[Chain]) {
        let tcx = self.analyzer.tcx;
        let mut seen: HashMap<rustc_hir::HirId, (usize, Span)> = HashMap::new();
        for (k, chain) in chains.iter().enumerate() {
            let Some(local) = chain.body.as_local() else {
                continue;
            };
            for captured in tcx.closure_captures(local) {
                let var = captured.get_root_variable();
                let ty = captured.place.ty();
                let Some(kind) = interior_kind(tcx, ty) else {
                    continue;
                };
                match seen.get(&var) {
                    Some((other, other_span)) if *other != k => {
                        let name = tcx.hir_name(var);
                        let other_span = *other_span;
                        self.report(
                            Severity::Note,
                            "interior-mutability",
                            function,
                            captured.get_path_span(tcx),
                            format!(
                                "psync runs concurrent chains that both use `{name}`, a `{kind}`: their reads and \
                                 updates now interleave instead of happening in program order"
                            ),
                            Some((other_span, "also used here".to_string())),
                        );
                    }
                    Some(_) => {}
                    None => {
                        seen.insert(var, (k, captured.get_path_span(tcx)));
                    }
                }
            }
        }
    }

    fn report(
        &mut self,
        severity: Severity,
        kind: &str,
        function: DefId,
        span: Span,
        message: String,
        secondary: Option<(Span, String)>,
    ) {
        if severity == Severity::Note && !self.notes {
            return;
        }
        let tcx = self.analyzer.tcx;
        let dcx = tcx.dcx();
        let mut diag = match severity {
            Severity::Warning if self.deny => dcx.struct_span_err(span, message.clone()),
            Severity::Warning => dcx.struct_span_warn(span, message.clone()),
            Severity::Note => dcx.struct_span_note(span, message.clone()),
        };
        if let Some((s, label)) = &secondary {
            diag.span_note(*s, label.clone());
        }
        diag.note(format!(
            "in the code `#[psync::graph]` generated for `{}`",
            display_path(tcx, function)
        ));
        diag.emit();
        let location = tcx.sess.source_map().span_to_diagnostic_string(span);
        let fix = (severity == Severity::Warning)
            .then(|| seq_fix(tcx, span, kind == "loop-speculative-write"))
            .flatten();
        self.findings.findings.push(Finding {
            severity,
            kind: kind.to_string(),
            message,
            location,
            function: display_path(tcx, function),
            fix,
        });
    }

    pub fn write_findings(&self) {
        let Some(dir) = std::env::var_os(env::FINDINGS).map(PathBuf::from) else {
            return;
        };
        let file = dir.join(format!("{}.json", self.findings.crate_name));
        // A crate with nothing concurrent left must not keep findings from an earlier run.
        if self.findings.checked_levels == 0 && self.findings.checked_loops == 0 {
            let _ = std::fs::remove_file(file);
            return;
        }
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string_pretty(&self.findings) {
            let _ = std::fs::write(file, json);
        }
    }
}

/// The body of an `async` block or async closure passed to a marker.
fn coroutine_of(ty: Ty<'_>) -> Option<DefId> {
    match ty.kind() {
        ty::Coroutine(def_id, _) | ty::CoroutineClosure(def_id, _) | ty::Closure(def_id, _) => {
            Some(*def_id)
        }
        _ => None,
    }
}

/// `Some("RefCell")` when `ty`, behind references and shared pointers, is a cell, lock or atomic.
fn interior_kind<'tcx>(tcx: TyCtxt<'tcx>, mut ty: Ty<'tcx>) -> Option<String> {
    loop {
        match ty.kind() {
            ty::Ref(_, inner, _) => ty = *inner,
            ty::Adt(def, args) => {
                let path = tcx.def_path_str(def.did());
                let last = path.rsplit("::").next().unwrap_or(&path).to_string();
                if matches!(last.as_str(), "Rc" | "Arc" | "Box") && !args.is_empty() {
                    ty = args.type_at(0);
                    continue;
                }
                let interior = matches!(
                    last.as_str(),
                    "Cell" | "RefCell" | "OnceCell" | "UnsafeCell" | "Mutex" | "RwLock"
                ) || last.starts_with("Atomic");
                return interior.then_some(last);
            }
            _ => return None,
        }
    }
}
