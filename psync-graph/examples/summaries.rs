//! Book, Part 0, "Effects and summaries" and "Dynamic dispatch".
//!
//! `cargo run -p psync-graph --example summaries`
//!
//! A toy program, as a call graph: each function has its own effects and calls. An effect
//! *summary* is everything a call to the function may do, through every callee. The driver
//! (`psync-check/src/driver/analysis.rs`) computes summaries from MIR in the same two ways shown
//! here: following calls with memoization, and resolving `dyn` calls by class-hierarchy analysis.
use std::collections::{BTreeMap, BTreeSet};

type Effects = BTreeSet<&'static str>;

enum Call {
    /// A call the compiler resolves to one function.
    Static(&'static str),
    /// A trait method called through `dyn Trait`: any implementation in the program may run.
    Dyn(&'static str),
}

struct Function {
    own: &'static [&'static str],
    calls: Vec<Call>,
    /// The trait method this function implements, if any (`impl Store for FileStore`).
    implements: Option<&'static str>,
}

fn program() -> BTreeMap<&'static str, Function> {
    use Call::*;
    BTreeMap::from([
        ("warm", Function { own: &["write file cache.json"], calls: vec![], implements: None }),
        ("load", Function { own: &["read file cache.json"], calls: vec![], implements: None }),
        // Mutual recursion: `walk` and `visit` call each other.
        ("walk", Function { own: &["write file walk.log"], calls: vec![Static("visit")], implements: None }),
        ("visit", Function { own: &["read net"], calls: vec![Static("walk")], implements: None }),
        ("snapshot", Function { own: &[], calls: vec![Dyn("Store::checkpoint")], implements: None }),
        ("FileStore::checkpoint", Function { own: &["write file state.db"], calls: vec![], implements: Some("Store::checkpoint") }),
        ("MemStore::checkpoint", Function { own: &[], calls: vec![], implements: Some("Store::checkpoint") }),
    ])
}

/// Class-hierarchy analysis over a closed world: the implementations of each trait method in the
/// whole program. The driver builds this table at link time, from every crate's summary file.
fn implementations(p: &BTreeMap<&'static str, Function>) -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut impls: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (name, f) in p {
        if let Some(m) = f.implements {
            impls.entry(m).or_default().push(*name);
        }
    }
    impls
}

fn callees<'a>(f: &'a Function, impls: &'a BTreeMap<&str, Vec<&'static str>>) -> Vec<&'static str> {
    f.calls
        .iter()
        .flat_map(|c| match c {
            Call::Static(g) => vec![*g],
            Call::Dyn(m) => impls.get(m).cloned().unwrap_or_default(),
        })
        .collect()
}

/// Memoized depth-first summaries, with a cycle cut: a function already being summarized
/// contributes nothing to the call that reached it again.
struct Memo<'p> {
    program: &'p BTreeMap<&'static str, Function>,
    impls: BTreeMap<&'static str, Vec<&'static str>>,
    memo: BTreeMap<&'static str, Effects>,
    in_progress: BTreeSet<&'static str>,
}

impl Memo<'_> {
    fn summarize(&mut self, name: &'static str) -> Effects {
        if let Some(s) = self.memo.get(name) {
            return s.clone();
        }
        if !self.in_progress.insert(name) {
            return Effects::new();
        }
        let f = &self.program[name];
        let mut s: Effects = f.own.iter().copied().collect();
        for g in callees(f, &self.impls) {
            s.extend(self.summarize(g));
        }
        self.in_progress.remove(name);
        self.memo.insert(name, s.clone());
        s
    }
}

/// Summaries as the least fixed point of `S(f) = own(f) ∪ ⋃ S(callees of f)`: start from the
/// own effects and repeat until nothing grows. Sets only grow and are bounded, so it terminates.
fn fixed_point(program: &BTreeMap<&'static str, Function>) -> BTreeMap<&'static str, Effects> {
    let impls = implementations(program);
    let mut s: BTreeMap<_, Effects> =
        program.iter().map(|(n, f)| (*n, f.own.iter().copied().collect())).collect();
    loop {
        let mut changed = false;
        for (name, f) in program {
            let mut grown = s[name].clone();
            for g in callees(f, &impls) {
                grown.extend(s[g].iter().copied());
            }
            if grown != s[name] {
                s.insert(name, grown);
                changed = true;
            }
        }
        if !changed {
            return s;
        }
    }
}

fn main() {
    let program = program();
    println!("class hierarchy: {:?}\n", implementations(&program));

    let mut memo = Memo {
        program: &program,
        impls: implementations(&program),
        memo: BTreeMap::new(),
        in_progress: BTreeSet::new(),
    };
    // Summarize `walk` first, as the driver would when it meets a call to it, then ask for `visit`.
    memo.summarize("walk");
    println!("memoized, cut cycles:");
    for name in ["walk", "visit", "snapshot", "load", "warm"] {
        println!("  {name:<9} {:?}", memo.summarize(name));
    }

    println!("\nleast fixed point:");
    let exact = fixed_point(&program);
    for name in ["walk", "visit", "snapshot", "load", "warm"] {
        println!("  {name:<9} {:?}", exact[name]);
    }
}
