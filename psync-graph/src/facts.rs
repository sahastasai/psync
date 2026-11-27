//! Facts from the compiler: what each call in an annotated function really does.
//!
//! The macro sees only tokens, so it judges calls by their names ([`crate::effects`]).
//! `cargo psync build` first runs `psync-driver`, which reads the compiled bodies of every callee
//! (MIR, across crates, with `dyn` calls resolved over the whole build) and writes one line per
//! call site of every `#[psync::graph]` function. When the environment variable `PSYNC_FACTS`
//! names that file, the macro looks each call up by the source location of the callee's name and
//! schedules with the compiler's answer instead of the name:
//!
//! - its effects replace the guessed ones: `factory.create(..)` that only reads is a read, a
//!   helper named `warm()` that writes `cache.json` is a write to that file;
//! - a callee that panics on purpose (`unwrap`, `expect`, `panic!`, `assert!`) is an exit, so
//!   writes after it wait for it.
//!
//! A fact is used only if the callee's name at that location still matches, so stale facts (from
//! before an edit) are ignored rather than misapplied. The generated code reads
//! `option_env!("PSYNC_FACTS_STAMP")`, which makes cargo rebuild annotated crates when the facts
//! change.

use crate::effects::{Domain, Effect, Kind, State, StateAt, Target};
use std::collections::HashMap;
use std::sync::OnceLock;

/// What the compiler found about one call.
#[derive(Clone, Debug)]
pub(crate) struct Fact {
    /// The callee's name as written, to detect stale facts.
    pub name: String,
    pub panics: bool,
    /// Some callee could not be seen through: the effects are a lower bound, so the name's guess
    /// still applies too.
    pub opaque: bool,
    pub effects: Vec<Effect>,
    /// For each effect: its target is the call's argument `j` (0 is a method's receiver).
    pub on_arg: Vec<Option<usize>>,
}

fn table() -> &'static HashMap<String, Fact> {
    static TABLE: OnceLock<HashMap<String, Fact>> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::env::var_os("PSYNC_FACTS")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse(&text))
            .unwrap_or_default()
    })
}

/// Parse the facts file written by `cargo psync` (see `psync_check::facts_text`).
pub(crate) fn parse(text: &str) -> HashMap<String, Fact> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut fields = line.splitn(5, '\t');
        let (Some(site), Some(name), Some(panics), Some(opaque), effects) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let (effects, on_arg): (Vec<Effect>, Vec<Option<usize>>) = effects
            .unwrap_or("")
            .split('\x1e')
            .filter(|e| !e.is_empty())
            .filter_map(|e| {
                let mut parts = e.splitn(4, '\x1f');
                let (domain, access, target) = (parts.next()?, parts.next()?, parts.next()?);
                // Malformed, it is ignored: the effect stays, on its target (empty: anything).
                let arg = parts.next().and_then(|j| j.parse::<usize>().ok());
                let (domain, ordered_domain) = match domain {
                    "fs" => (Domain::Fs, false),
                    "net" => (Domain::Net, false),
                    "env" => (Domain::Env, false),
                    "process" => (Domain::Process, false),
                    "output" => (Domain::Service(Some("type:stdout".into())), true),
                    // Hidden state: a static or thread-local by path, or (empty) the state behind
                    // the argument in the fourth field, or any state.
                    st if st.starts_with("state:") || st.starts_with("observed:") => {
                        let (observed, path) = match st.strip_prefix("observed:") {
                            Some(p) => (true, p),
                            None => (false, &st["state:".len()..]),
                        };
                        let at = if path.is_empty() {
                            StateAt::Any
                        } else {
                            StateAt::Static(path.to_string())
                        };
                        (Domain::State(State { at, observed }), false)
                    }
                    svc => (
                        Domain::Service(Some(format!("type:{}", svc.strip_prefix("svc:")?))),
                        false,
                    ),
                };
                let (kind, ordered) = match access {
                    "r" => (Kind::Read, false),
                    "w" => (Kind::Write, ordered_domain),
                    "o" => (Kind::Write, true),
                    _ => return None,
                };
                Some((
                    Effect {
                        domain,
                        kind,
                        target: if target.is_empty() {
                            Target::Any
                        } else {
                            Target::Literal(target.to_string())
                        },
                        op: format!("{name}, per the compiler"),
                        ordered,
                        source: None,
                    },
                    arg,
                ))
            })
            .unzip();
        let fact = Fact {
            name: name.to_string(),
            panics: panics == "1",
            opaque: opaque == "1",
            effects,
            on_arg,
        };
        // A site listed twice (two builds of one crate): keep everything either says.
        match out.get_mut(site) {
            Some(old) => {
                let old: &mut Fact = old;
                old.panics |= fact.panics;
                old.opaque |= fact.opaque;
                old.effects.extend(fact.effects);
                old.on_arg.extend(fact.on_arg);
            }
            None => {
                out.insert(site.to_string(), fact);
            }
        }
    }
    out
}

/// The fact for the call whose callee name is `ident` (a method name, or the last segment of a
/// function path) starting at `span`, if the compiler recorded one there.
pub(crate) fn at(span: proc_macro2::Span, ident: &str) -> Option<&'static Fact> {
    #[cfg(feature = "cli")]
    if let Some((table, file)) = CLI.with(|c| c.borrow().clone()) {
        let start = span.start();
        let key = format!("{file}:{}:{}", start.line, start.column + 1);
        return table.0.get(&key).filter(|f| f.name == ident);
    }
    let table = table();
    if table.is_empty() {
        return None;
    }
    let key = site_key(span)?;
    table.get(&key).filter(|f| f.name == ident)
}

/// Facts parsed for a command-line tool (`psync-cert`), keyed as in the file.
#[cfg(feature = "cli")]
pub struct FactTable(HashMap<String, Fact>);

#[cfg(feature = "cli")]
impl FactTable {
    /// Parse facts files (see `psync_check::facts_text`); the table lives as long as the program.
    pub fn parse(text: &str) -> &'static FactTable {
        Box::leak(Box::new(FactTable(parse(text))))
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(feature = "cli")]
thread_local! {
    static CLI: std::cell::RefCell<Option<(&'static FactTable, String)>> =
        const { std::cell::RefCell::new(None) };
}

/// For a command-line tool: from now on, look up the calls of functions parsed from `file` in
/// `table`, under keys `file:line:column` (column from 1, as the driver writes them). `None`
/// turns facts off again.
#[cfg(feature = "cli")]
pub fn use_facts(facts: Option<(&'static FactTable, String)>) {
    CLI.with(|c| *c.borrow_mut() = facts);
}

/// `/abs/path/file.rs:line:column` of a span's start, as the driver writes it. Only available
/// inside a procedural macro.
fn site_key(span: proc_macro2::Span) -> Option<String> {
    if !proc_macro::is_available() {
        return None;
    }
    let span = span.unwrap();
    let path = span.local_file()?;
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    Some(format!(
        "{}:{}:{}",
        path.display(),
        span.line(),
        span.column()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fourth field: the effect's target is the call's argument `j`.
    #[test]
    fn parses_argument_targets() {
        let t = parse("/a.rs:3:5\tstash\t0\t0\tfs\x1fw\x1f\x1f0\x1efs\x1fr\x1fa.json\n");
        let f = &t["/a.rs:3:5"];
        assert_eq!(f.on_arg, [Some(0), None]);
        assert!(matches!(&f.effects[1].target, Target::Literal(l) if l == "a.json"));
        // A malformed argument leaves the effect on anything: never fewer effects.
        let f = &parse("/a.rs:1:1\tx\t0\t0\tfs\x1fw\x1f\x1fz\n")["/a.rs:1:1"];
        assert_eq!(f.on_arg, [None]);
        assert!(matches!(f.effects[0].target, Target::Any));
    }

    /// Hidden state: a static by path, the state behind an argument, observed state.
    #[test]
    fn parses_state() {
        use crate::effects::{State, StateAt};
        let t = parse(
            "/a.rs:3:5\tbump\t0\t0\tstate:\x1fw\x1f\x1f0\x1estate:graph::PEAK\x1fw\x1f\x1eobserved:graph::TICKS\x1fr\x1f\n",
        );
        let f = &t["/a.rs:3:5"];
        let at = |i: usize| match &f.effects[i].domain {
            Domain::State(State { at, observed }) => (at.clone(), *observed),
            d => panic!("{d:?}"),
        };
        assert_eq!(at(0), (StateAt::Any, false));
        assert_eq!(f.on_arg[0], Some(0));
        assert_eq!(at(1), (StateAt::Static("graph::PEAK".into()), false));
        assert_eq!(at(2), (StateAt::Static("graph::TICKS".into()), true));
        assert!(f.effects[1].writes() && !f.effects[2].writes());
        // Two writes of one static conflict, of two statics do not, observed state never does.
        let e = |i: usize| f.effects[i].clone();
        assert!(crate::effects::conflict(&e(1), &e(1)).is_some());
        let mut other = e(1);
        other.domain = Domain::State(State {
            at: StateAt::Static("graph::ACTIVE".into()),
            observed: false,
        });
        assert!(crate::effects::conflict(&e(1), &other).is_none());
        assert!(crate::effects::conflict(&e(2), &e(1)).is_none());
        // Any state may be every static.
        assert!(crate::effects::conflict(&e(0), &e(1)).is_some());
        // Places: `h/graph/PEAK`, and `h` for any.
        assert_eq!(e(1).place(), ["h", "graph", "PEAK"]);
        assert_eq!(e(0).place(), ["h"]);
    }

    #[test]
    fn parses_lines() {
        let text = "/a.rs:3:5\tcreate\t1\t0\tfs\x1fw\x1fcache.json\x1esvc:kube::Api::<K>\x1fr\x1f\n\
                    garbage\n";
        let t = parse(text);
        let f = &t["/a.rs:3:5"];
        assert_eq!(f.name, "create");
        assert!(f.panics);
        assert_eq!(f.effects.len(), 2);
        assert!(f.effects[0].writes() && f.effects[0].domain == Domain::Fs);
        assert_eq!(
            f.effects[1].domain,
            Domain::Service(Some("type:kube::Api::<K>".into()))
        );
    }
}
