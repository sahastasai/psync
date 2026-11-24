//! Book, Part 0, "Dependencies and dataflow".
//!
//! `cargo run -p psync-graph --example dataflow [-- --forward]`
//!
//! Part A is psync's scheduling algorithm in miniature (the real one is `plan` in
//! `psync-graph/src/schedule.rs`): read and write sets, edges, chains and levels.
//!
//! Part B is the classic dataflow analysis a compiler runs over a control-flow graph, liveness,
//! solved by iterating to a fixed point. psync's region graphs have no cycles and need no fixed
//! point; MIR's control-flow graphs do, and so do effect summaries of recursive functions.
use std::collections::BTreeSet;

type Set = BTreeSet<&'static str>;

fn set(items: &[&'static str]) -> Set {
    items.iter().copied().collect()
}

/// One statement of a region, as the access analysis describes it.
struct Op {
    text: &'static str,
    defs: Set,   // names it binds
    uses: Set,   // names it reads
    writes: Set, // places it mutates (`lens` for `lens.push(..)`)
    awaits: bool,
}

/// Why `b`, later in the program, must wait for `a`. The same questions, in the same order, as
/// `depends` in `schedule.rs` (which also asks about effects and exits).
fn depends(a: &Op, b: &Op) -> Option<String> {
    if let Some(x) = a.defs.intersection(&b.uses).next() {
        return Some(format!("reads `{x}`")); // true dependence: read after write
    }
    if let Some(x) = a.defs.intersection(&b.defs).next() {
        return Some(format!("rebinds `{x}`")); // output dependence
    }
    if let Some(x) = a.uses.intersection(&b.defs).next() {
        return Some(format!("shadows `{x}`")); // anti-dependence: write after read
    }
    let b_touches: Set = b.uses.union(&b.writes).copied().collect();
    if let Some(x) = a.writes.intersection(&b_touches).next() {
        return Some(format!("writes `{x}`"));
    }
    if let Some(x) = b.writes.intersection(&a.uses).next() {
        return Some(format!("writes `{x}`"));
    }
    None
}

fn part_a() {
    // The `hoisted` example of the Basic concepts chapter, after hoisting.
    let hoisted = [
        Op {
            text: "let mut lens = Vec::new();",
            defs: set(&["lens"]),
            uses: set(&[]),
            writes: set(&[]),
            awaits: false,
        },
        Op {
            text: "let h2 = api.len(&names[0]).await;",
            defs: set(&["h2"]),
            uses: set(&["api", "names"]),
            writes: set(&[]),
            awaits: true,
        },
        Op {
            text: "lens.push(h2);",
            defs: set(&[]),
            uses: set(&["lens", "h2"]),
            writes: set(&["lens"]),
            awaits: false,
        },
        Op {
            text: "let s3 = api.kind(&names[1]).await;",
            defs: set(&["s3"]),
            uses: set(&["api", "names"]),
            writes: set(&[]),
            awaits: true,
        },
        Op {
            text: "match s3 { .. => lens.push(..) }",
            defs: set(&[]),
            uses: set(&["lens", "s3"]),
            writes: set(&["lens"]),
            awaits: false,
        },
    ];
    println!("Part A: the dependency graph of `hoisted`");
    schedule(&hoisted);
    // The `chains` test of `tests/graph.rs`: two per-item pipelines.
    let chains = [
        Op {
            text: "let a1 = io(T, 1).await;",
            defs: set(&["a1"]),
            uses: set(&[]),
            writes: set(&[]),
            awaits: true,
        },
        Op {
            text: "let a2 = io(2 * T, 2).await;",
            defs: set(&["a2"]),
            uses: set(&[]),
            writes: set(&[]),
            awaits: true,
        },
        Op {
            text: "let b1 = io(2 * T, a1 + 10).await;",
            defs: set(&["b1"]),
            uses: set(&["a1"]),
            writes: set(&[]),
            awaits: true,
        },
        Op {
            text: "let b2 = io(T, a2 + 10).await;",
            defs: set(&["b2"]),
            uses: set(&["a2"]),
            writes: set(&[]),
            awaits: true,
        },
    ];
    println!("\nPart A: the dependency graph of `chains`");
    schedule(&chains);
}

/// Edges, chains and levels of one region.
fn schedule(ops: &[Op]) {
    let n = ops.len();
    let (mut preds, mut succs) = (vec![vec![]; n], vec![vec![]; n]);
    for (i, op) in ops.iter().enumerate() {
        println!(
            "  n{i}: {}{}",
            op.text,
            if op.awaits { "  [await]" } else { "" }
        );
    }
    for j in 0..n {
        for i in 0..j {
            if let Some(why) = depends(&ops[i], &ops[j]) {
                println!("  n{i} -> n{j} ({why})");
                preds[j].push(i);
                succs[i].push(j);
            }
        }
    }
    // Chains: fuse u -> v when it is the only edge into v and the only edge out of u. (psync
    // fuses in one more case; see `plan`.)
    let mut chain_of = vec![0; n];
    let mut chains: Vec<Vec<usize>> = vec![];
    for v in 0..n {
        match preds[v][..] {
            [u] if succs[u].len() == 1 && chains[chain_of[u]].last() == Some(&u) => {
                chains[chain_of[u]].push(v);
                chain_of[v] = chain_of[u];
            }
            _ => {
                chain_of[v] = chains.len();
                chains.push(vec![v]);
            }
        }
    }
    for (c, members) in chains.iter().enumerate() {
        let names: Vec<String> = members.iter().map(|v| format!("n{v}")).collect();
        println!("  chain {c}: {}", names.join(" -> "));
    }
    // Levels: a chain runs one level after the deepest chain it waits for. Chains are numbered in
    // program order and edges only go forward, so one pass in order suffices (a topological
    // order for free).
    let mut level = vec![0; chains.len()];
    for c in 0..chains.len() {
        for &v in &chains[c] {
            for &p in &preds[v] {
                if chain_of[p] != c {
                    level[c] = level[c].max(level[chain_of[p]] + 1);
                }
            }
        }
    }
    for l in 0..=level.iter().copied().max().unwrap_or(0) {
        let here: Vec<String> = (0..chains.len())
            .filter(|&c| level[c] == l)
            .map(|c| {
                chains[c]
                    .iter()
                    .map(|v| format!("n{v}"))
                    .collect::<Vec<_>>()
                    .join("->")
            })
            .collect();
        println!("  level {l}: {}", here.join(" || "));
    }
}

/// A basic block of a control-flow graph: what it reads before writing (`gen`), what it writes
/// (`kill`), and where control goes next.
struct Block {
    name: &'static str,
    uses: Set,
    defs: Set,
    next: Vec<usize>,
}

fn part_b() {
    // fn sum(v) { let mut i = 0; let mut s = 0; while i < v.len() { s += v[i]; i += 1; } s }
    let cfg = [
        Block {
            name: "bb0: i = 0; s = 0",
            uses: set(&[]),
            defs: set(&["i", "s"]),
            next: vec![1],
        },
        Block {
            name: "bb1: if i < v.len()",
            uses: set(&["i", "v"]),
            defs: set(&[]),
            next: vec![2, 3],
        },
        Block {
            name: "bb2: s += v[i]; i += 1",
            uses: set(&["s", "v", "i"]),
            defs: set(&["s", "i"]),
            next: vec![1],
        },
        Block {
            name: "bb3: return s",
            uses: set(&["s"]),
            defs: set(&[]),
            next: vec![],
        },
    ];
    // live_in(b) = uses(b) ∪ (live_out(b) − defs(b));  live_out(b) = ∪ live_in(successors)
    // Start from "nothing is live" and repeat until no set changes: the least fixed point.
    // Liveness flows backwards, so visiting blocks last to first converges fastest. `--forward`
    // visits them first to last: the same answer, in more rounds.
    let forward = std::env::args().any(|a| a == "--forward");
    let order: Vec<usize> = if forward {
        (0..cfg.len()).collect()
    } else {
        (0..cfg.len()).rev().collect()
    };
    let mut live_in = vec![Set::new(); cfg.len()];
    let mut round = 0;
    loop {
        round += 1;
        let mut changed = false;
        for &b in &order {
            let block = &cfg[b];
            let out: Set = block
                .next
                .iter()
                .flat_map(|&s| live_in[s].iter().copied())
                .collect();
            let new: Set = block
                .uses
                .union(&out.difference(&block.defs).copied().collect())
                .copied()
                .collect();
            if new != live_in[b] {
                live_in[b] = new;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let direction = if forward { "forward" } else { "backward" };
    println!("\nPart B: liveness, visiting blocks {direction}, fixed point after {round} rounds");
    for (b, block) in cfg.iter().enumerate() {
        println!("  {:<26} live on entry: {:?}", block.name, live_in[b]);
    }
}

fn main() {
    part_a();
    part_b();
}
