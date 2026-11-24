//! `cargo psync check`: verifies `#[psync::graph]` rewrites against the compiled bodies of callees.
//!
//! `#[psync::graph]` sees only the tokens of the function it rewrites. It judges every call by its
//! name: `api.get(..)` reads, `store.put(..)` writes, `do_work(x)` is unknown. `cargo psync check`
//! runs the compiler with a custom driver (`psync-driver`, like `clippy-driver`) that sees the
//! program after type checking, as MIR:
//!
//! 1. **Summaries.** For every function of every crate in the build, the driver computes which
//!    effects on the outside world the function performs: file reads and writes (`std::fs`,
//!    `tokio::fs`, ...), sockets, processes, environment variables, and calls into other crates'
//!    APIs, classified by their verbs (`delete`, `list`, ...). Calls are resolved exactly (trait
//!    methods to their impls), callees in the same crate are followed into their bodies, and
//!    summaries of dependencies are read from files the driver wrote when it compiled them.
//! 2. **Checks.** In every function `#[psync::graph]` rewrote, the generated code marks each
//!    concurrent level and each prefetched loop (`maybe_done_at`, `over_at`) with what the macro
//!    assumed. The driver compares those assumptions with the summaries and warns where the
//!    rewrite runs two conflicting effects at once, performs a write before an earlier exit, or
//!    shares interior-mutable state between concurrent chains.
//!
//! This library holds the parts shared by the driver and the `cargo psync` front end: the effect
//! model, summary files and findings. It does not link the compiler.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Where an effect lands.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Domain {
    /// The file system.
    Fs,
    /// Sockets used directly (`std::net`, `tokio::net`).
    Net,
    /// Environment variables and the working directory.
    Env,
    /// A child process: it may touch anything.
    Process,
    /// Standard output and error: the order of writes is visible.
    Output,
    /// Another crate's API, named by the type or module it belongs to (`kube::Api::<K>`).
    Service(String),
}

/// How an effect touches its domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Access {
    Read,
    Write,
    /// A write whose order relative to others on the same domain is visible (`send`, `append`).
    Ordered,
}

/// One effect of a function, with the calls through which it happens.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Effect {
    pub domain: Domain,
    pub access: Access,
    /// A constant target (a file path or key), when the call's argument is a string literal.
    pub target: Option<String>,
    /// The call chain from the summarized function to the operation, outermost first.
    pub via: Vec<String>,
}

impl Effect {
    pub fn writes(&self) -> bool {
        self.access != Access::Read
    }

    /// `write file "cfg.toml"` / `read kube::Api::<K>`.
    pub fn describe(&self) -> String {
        let access = match self.access {
            Access::Read => "read",
            Access::Write => "write",
            Access::Ordered => "ordered write",
        };
        let domain = match &self.domain {
            Domain::Fs => "file".to_string(),
            Domain::Net => "socket".to_string(),
            Domain::Env => "environment".to_string(),
            Domain::Process => "process".to_string(),
            Domain::Output => "standard output".to_string(),
            Domain::Service(s) => format!("`{s}`"),
        };
        match &self.target {
            Some(t) => format!("{access} {domain} {t:?}"),
            None => format!("{access} {domain}"),
        }
    }

    /// `helper -> std::fs::write`.
    pub fn path(&self) -> String {
        self.via.join(" -> ")
    }
}

/// Whether two effects may not run concurrently: one writes, they may reach the same resource,
/// and their targets may be the same (or both are ordered writes on one domain).
pub fn conflict(a: &Effect, b: &Effect) -> bool {
    if !(a.writes() || b.writes()) {
        return false;
    }
    let same_domain = match (&a.domain, &b.domain) {
        // A child process cannot change this process's environment, but may touch anything else.
        (Domain::Process, Domain::Env) | (Domain::Env, Domain::Process) => {
            return a.domain == Domain::Env && a.writes() || b.domain == Domain::Env && b.writes();
        }
        (Domain::Process, _) | (_, Domain::Process) => return true,
        (x, y) => x == y,
    };
    if !same_domain {
        return false;
    }
    if a.access == Access::Ordered && b.access == Access::Ordered {
        return true;
    }
    match (&a.target, &b.target) {
        (Some(x), Some(y)) if a.domain == Domain::Fs => paths_overlap(x, y),
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// Equal paths, or one is a directory containing the other.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    a == b
        || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
        || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
}

/// The summary of one function, as stored for other crates.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FnSummary {
    /// The function's path, for people.
    pub path: String,
    pub effects: Vec<Effect>,
    /// Calls the driver could not see through (function pointers, `dyn` methods without a
    /// verb, other crates' functions without a summary).
    pub opaque: Vec<String>,
    /// It may panic explicitly (`unwrap`, `expect`, `panic!`, `assert!`) in the user's code,
    /// directly or through the user's own functions.
    #[serde(default)]
    pub panics: bool,
    /// Trait methods it calls through `dyn` or an unresolved generic, by key: their effects
    /// are the union over every implementation in the build, resolved by [`link`].
    #[serde(default)]
    pub deferred: Vec<String>,
}

/// What one call in a `#[psync::graph]` function does, for the macro.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallFact {
    /// The callee's path, as resolved by the compiler.
    pub callee: String,
    /// The callee's name as written at the call site, to detect stale facts.
    pub name: String,
    pub effects: Vec<Effect>,
    pub panics: bool,
    pub deferred: Vec<String>,
    /// Some of its callees could not be seen through.
    pub opaque: bool,
}

/// Every summary of one crate, keyed by the stable hash of each function's definition path.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CrateSummaries {
    pub crate_name: String,
    /// The compiler's stable id of the crate: a function of a summarized crate that is missing
    /// from `functions` has no effects.
    #[serde(default)]
    pub crate_id: String,
    pub functions: BTreeMap<String, FnSummary>,
    /// Trait method key -> keys of this crate's methods implementing it.
    #[serde(default)]
    pub impls: BTreeMap<String, Vec<String>>,
    /// Call sites in `#[psync::graph]` functions, by `file:line:column` of the callee's name.
    #[serde(default)]
    pub calls: BTreeMap<String, CallFact>,
}

/// The effects of a deferred trait method: the union over its implementations in the build.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Resolved {
    pub effects: Vec<Effect>,
    pub panics: bool,
    /// Implementations found.
    pub impls: usize,
}

/// Link-time resolution of `dyn` and generic trait calls over the whole build (closed world):
/// every deferred trait method's effects are the union of its implementations' effects, and
/// of what those call through `dyn` in turn.
pub fn link(crates: &[CrateSummaries]) -> BTreeMap<String, Resolved> {
    let mut functions: BTreeMap<&str, &FnSummary> = BTreeMap::new();
    let mut impls: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for c in crates {
        for (k, f) in &c.functions {
            functions.insert(k, f);
        }
        for (t, is) in &c.impls {
            impls
                .entry(t)
                .or_default()
                .extend(is.iter().map(String::as_str));
        }
    }
    let mut out: BTreeMap<String, Resolved> = BTreeMap::new();
    let mut traits: Vec<&str> = impls.keys().copied().collect();
    for f in functions.values() {
        traits.extend(f.deferred.iter().map(String::as_str));
    }
    traits.sort();
    traits.dedup();
    for t in traits {
        let mut resolved = Resolved::default();
        let mut stack = vec![t];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(t) = stack.pop() {
            if !seen.insert(t) {
                continue;
            }
            for i in impls.get(t).into_iter().flatten() {
                resolved.impls += 1;
                // An implementation without a summary has no effects (summaries omit those).
                let Some(f) = functions.get(i) else { continue };
                resolved.panics |= f.panics;
                for e in &f.effects {
                    if !resolved.effects.iter().any(|x| same_effect(x, e)) {
                        resolved.effects.push(e.clone());
                    }
                }
                stack.extend(f.deferred.iter().map(String::as_str));
            }
        }
        out.insert(t.to_string(), resolved);
    }
    out
}

fn same_effect(a: &Effect, b: &Effect) -> bool {
    a.domain == b.domain && a.access == b.access && a.target == b.target
}

/// The final effects of a call site: its own, and those of the trait methods it reaches
/// through `dyn`.
pub fn resolve_call(fact: &CallFact, linked: &BTreeMap<String, Resolved>) -> (Vec<Effect>, bool) {
    let mut effects = fact.effects.clone();
    let mut panics = fact.panics;
    for t in &fact.deferred {
        if let Some(r) = linked.get(t) {
            panics |= r.panics;
            for e in &r.effects {
                if !effects.iter().any(|x| same_effect(x, e)) {
                    let mut e = e.clone();
                    e.via.insert(0, fact.callee.clone());
                    effects.push(e);
                }
            }
        }
    }
    (effects, panics)
}

/// The facts file the macro reads (`PSYNC_FACTS`): one call site per line,
/// `site \t name \t panics \t effects`, effects separated by `\x1e`, each
/// `domain \x1f access \x1f target`. Domains: `fs`, `net`, `env`, `process`, `output`,
/// `svc:<owner>`; accesses `r`, `w`, `o`. A line-based format keeps the macro free of a JSON
/// dependency.
pub fn facts_text(crates: &[CrateSummaries], linked: &BTreeMap<String, Resolved>) -> String {
    let mut out = String::new();
    for c in crates {
        for (site, fact) in &c.calls {
            let (effects, panics) = resolve_call(fact, linked);
            let effects: Vec<String> = effects
                .iter()
                .map(|e| {
                    let domain = match &e.domain {
                        Domain::Fs => "fs".to_string(),
                        Domain::Net => "net".to_string(),
                        Domain::Env => "env".to_string(),
                        Domain::Process => "process".to_string(),
                        Domain::Output => "output".to_string(),
                        Domain::Service(s) => format!("svc:{s}"),
                    };
                    let access = match e.access {
                        Access::Read => "r",
                        Access::Write => "w",
                        Access::Ordered => "o",
                    };
                    format!(
                        "{domain}\x1f{access}\x1f{}",
                        e.target.as_deref().unwrap_or("")
                    )
                })
                .collect();
            out += &format!(
                "{site}\t{}\t{}\t{}\n",
                fact.name,
                u8::from(panics),
                effects.join("\x1e")
            );
        }
    }
    out
}

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// The rewrite changes what the program does to the outside world.
    Warning,
    /// The order of visible output, or of operations behind interior mutability, may change.
    Note,
}

/// One problem found in a rewritten function.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// `conflict`, `speculative-write`, `loop-speculative-write`, `interior-mutability`, `output-order`.
    pub kind: String,
    pub message: String,
    /// `file:line:column` of the primary location.
    pub location: String,
    /// The function the rewritten code is in.
    pub function: String,
    /// A machine-applicable fix: text to insert at a byte offset of a file (`cargo psync fix`).
    #[serde(default)]
    pub fix: Option<Fix>,
}

/// An insertion that fixes a finding: `#[psync::seq]` before the statement or loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub file: String,
    pub offset: usize,
    pub insert: String,
}

/// The findings of one crate, as written by the driver.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CrateFindings {
    pub crate_name: String,
    pub checked_levels: usize,
    pub checked_loops: usize,
    /// Per rewritten function: `(concurrent levels, prefetched loops)`.
    #[serde(default)]
    pub functions: BTreeMap<String, (usize, usize)>,
    pub findings: Vec<Finding>,
}

/// Read every `*.json` file in `dir` as `T`, skipping files that do not parse.
pub fn read_all<T: for<'de> Deserialize<'de>>(dir: &Path) -> Vec<T> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str(&text)
        {
            out.push(value);
        }
    }
    out
}

/// Environment variables the front end passes to the driver.
pub mod env {
    /// Directory of per-crate summary files.
    pub const SUMMARIES: &str = "PSYNC_SUMMARY_DIR";
    /// Directory of per-crate findings files.
    pub const FINDINGS: &str = "PSYNC_FINDINGS_DIR";
    /// Set to `1` to make findings errors.
    pub const DENY: &str = "PSYNC_CHECK_DENY";
    /// Set to `1` to also report notes (output order, interior mutability).
    pub const NOTES: &str = "PSYNC_CHECK_NOTES";
    /// The linked resolution of deferred trait methods (`linked.json`), when a previous run
    /// produced one: the driver then checks `dyn` calls against every implementation.
    pub const LINKED: &str = "PSYNC_LINKED";
    /// Read by the macro: the facts file to schedule with.
    pub const FACTS: &str = "PSYNC_FACTS";
    /// Read by the macro (through `option_env!`, so cargo rebuilds when it changes): a hash of
    /// the facts file.
    pub const FACTS_STAMP: &str = "PSYNC_FACTS_STAMP";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(domain: Domain, access: Access, target: Option<&str>) -> Effect {
        Effect {
            domain,
            access,
            target: target.map(Into::into),
            via: vec![],
        }
    }

    #[test]
    fn conflicts() {
        let w = fx(Domain::Fs, Access::Write, Some("out/a"));
        assert!(conflict(&w, &fx(Domain::Fs, Access::Read, Some("out"))));
        assert!(!conflict(&w, &fx(Domain::Fs, Access::Read, Some("other"))));
        assert!(conflict(&w, &fx(Domain::Fs, Access::Read, None)));
        assert!(!conflict(
            &fx(Domain::Fs, Access::Read, None),
            &fx(Domain::Fs, Access::Read, None)
        ));
        assert!(conflict(&fx(Domain::Process, Access::Write, None), &w));
        let api = |a| fx(Domain::Service("kube::Api".into()), a, None);
        assert!(conflict(&api(Access::Write), &api(Access::Read)));
        assert!(!conflict(
            &api(Access::Write),
            &fx(Domain::Service("s3".into()), Access::Write, None)
        ));
    }
}
