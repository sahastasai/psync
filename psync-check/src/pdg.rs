//! Control dependence for a function's program dependence graph (Ferrante, Ottenstein and
//! Warren, 1987).
//!
//! A node `n` is **control dependent** on the edge `a -> b` when `b` leads to `n` on every path to
//! the exit (`n` post-dominates `b`), but `a` does not (`n` does not post-dominate `a`): the branch
//! at `a` decides whether `n` runs. Post-dominators are dominators of the reversed graph, from a
//! virtual exit that every exit block leads to (Cooper, Harvey and Kennedy's iterative algorithm).

use serde::{Deserialize, Serialize};

/// A control-flow graph: `succ[n]` are the blocks `n` may jump to, each with a label for the edge
/// (`true`, `Err`, `otherwise`). Blocks without successors are exits.
#[derive(Clone, Debug, Default)]
pub struct Cfg {
    pub succ: Vec<Vec<(usize, String)>>,
}

/// The immediate post-dominator of each block: `Some(n)` for a block, `None` for the virtual exit
/// (the block is an exit, or every path from it ends there directly) or for a block from which no
/// exit is reachable.
pub fn post_dominators(cfg: &Cfg) -> Vec<Option<usize>> {
    let n = cfg.succ.len();
    let exit = n; // virtual
    // Reverse postorder of the reversed graph, from the exit: predecessors in the reversed graph
    // are successors in the original.
    let mut preds_rev: Vec<Vec<usize>> = vec![Vec::new(); n + 1]; // reversed-graph successors
    for (a, s) in cfg.succ.iter().enumerate() {
        if s.is_empty() {
            preds_rev[exit].push(a);
        }
        for (b, _) in s {
            preds_rev[*b].push(a);
        }
    }
    let mut order = Vec::new();
    let mut seen = vec![false; n + 1];
    let mut stack = vec![(exit, 0usize)];
    seen[exit] = true;
    while let Some((v, i)) = stack.pop() {
        if i < preds_rev[v].len() {
            stack.push((v, i + 1));
            let w = preds_rev[v][i];
            if !seen[w] {
                seen[w] = true;
                stack.push((w, 0));
            }
        } else {
            order.push(v);
        }
    }
    order.reverse(); // reverse postorder, exit first
    let mut rpo = vec![usize::MAX; n + 1];
    for (k, &v) in order.iter().enumerate() {
        rpo[v] = k;
    }
    let mut idom: Vec<Option<usize>> = vec![None; n + 1];
    idom[exit] = Some(exit);
    let intersect = |idom: &[Option<usize>], mut a: usize, mut b: usize| {
        while a != b {
            while rpo[a] > rpo[b] {
                a = idom[a].unwrap();
            }
            while rpo[b] > rpo[a] {
                b = idom[b].unwrap();
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &v in order.iter().skip(1) {
            // Predecessors of `v` in the reversed graph: its successors, or the exit.
            let ps: Vec<usize> = if cfg.succ[v].is_empty() {
                vec![exit]
            } else {
                cfg.succ[v].iter().map(|(b, _)| *b).collect()
            };
            let mut new: Option<usize> = None;
            for p in ps {
                if idom[p].is_some() {
                    new = Some(match new {
                        None => p,
                        Some(x) => intersect(&idom, p, x),
                    });
                }
            }
            if new.is_some() && idom[v] != new {
                idom[v] = new;
                changed = true;
            }
        }
    }
    (0..n).map(|v| idom[v].filter(|&d| d != exit)).collect()
}

/// One control dependence: the block runs only when the branch at `branch` takes the edge
/// labelled `label`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guard {
    pub branch: usize,
    pub label: String,
}

/// The guards of each block: the branches it is control dependent on.
pub fn control_dependence(cfg: &Cfg) -> Vec<Vec<Guard>> {
    let ipdom = post_dominators(cfg);
    let n = cfg.succ.len();
    let mut out: Vec<Vec<Guard>> = vec![Vec::new(); n];
    for (a, s) in cfg.succ.iter().enumerate() {
        if s.len() < 2 {
            continue;
        }
        for (b, label) in s {
            // Walk up the post-dominator tree from `b` to `a`'s immediate post-dominator: those
            // blocks run exactly when this edge is taken.
            let mut runner = Some(*b);
            let mut steps = 0;
            while let Some(r) = runner {
                if Some(r) == ipdom[a] || steps > n {
                    break;
                }
                let g = Guard {
                    branch: a,
                    label: label.clone(),
                };
                if !out[r].contains(&g) {
                    out[r].push(g);
                }
                runner = ipdom[r];
                steps += 1;
            }
        }
    }
    out
}

/// A point of interest in a function: a call with effects or a call of the crate's own code, an
/// await, or a return.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdgNode {
    pub block: usize,
    pub line: usize,
    /// `call`, `await` or `return`.
    pub kind: String,
    /// The callee, for a call.
    pub callee: String,
    /// What the call does, as the summaries describe it (`write file arg 0`).
    pub effects: Vec<String>,
    pub guards: Vec<Guard>,
    /// The calls (by block) whose results flow into this call's arguments.
    #[serde(default)]
    pub uses: Vec<usize>,
}

/// A branch: where it is and what it tests.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdgBranch {
    pub block: usize,
    pub line: usize,
    pub test: String,
    /// The calls (by block) whose results the test depends on.
    #[serde(default)]
    pub uses: Vec<usize>,
}

/// One chain of a join the macro generated: a closure or async block of its own, whose graph is
/// listed under `path`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdgChain {
    pub path: String,
    /// Its first and last statements, by position in the rewritten block.
    pub first: u64,
    pub last: u64,
    pub exits: bool,
    pub speculate: bool,
    pub independent: bool,
    /// A job on a thread (Phase II), not a future.
    pub sync: bool,
}

/// Chains that run concurrently: one join of the rewritten function.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdgJoin {
    pub level: u64,
    pub chains: Vec<PdgChain>,
}

/// A prefetched loop: `path` (the prefix) runs for later items while earlier ones finish.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdgPrefetch {
    pub path: String,
    pub terminates: bool,
    pub speculate: bool,
}

/// The dependence graph of one body (a function, closure or async block).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FnPdg {
    pub path: String,
    pub nodes: Vec<PdgNode>,
    pub branches: Vec<PdgBranch>,
    /// The joins the macro generated in this body.
    #[serde(default)]
    pub joins: Vec<PdgJoin>,
    #[serde(default)]
    pub prefetches: Vec<PdgPrefetch>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CratePdg {
    pub crate_name: String,
    pub functions: Vec<FnPdg>,
}

/// A function's graph as text: each node with the branches it depends on.
///
/// ```text
/// app::keep::{closure#0}
///   b2  line 61  await
///   b5  line 62  call std::fs::write  [write file arg 1]  if b3 (line 60, match on `Result`) = Ok
/// ```
pub fn render(f: &FnPdg) -> String {
    let blocks = |bs: &[usize]| {
        bs.iter()
            .map(|b| format!("b{b}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!("{}\n", f.path);
    for j in &f.joins {
        let chains: Vec<String> = j
            .chains
            .iter()
            .map(|c| {
                let mut tags = vec![if c.first == c.last {
                    format!("statement {}", c.first)
                } else {
                    format!("statements {}-{}", c.first, c.last)
                }];
                for (on, tag) in [
                    (c.exits, "may exit"),
                    (c.speculate, "speculate"),
                    (c.independent, "independent"),
                    (c.sync, "thread"),
                ] {
                    if on {
                        tags.push(tag.into());
                    }
                }
                format!("{} ({})", c.path, tags.join(", "))
            })
            .collect();
        out += &format!("  join {}: {}\n", j.level, chains.join(" || "));
    }
    for p in &f.prefetches {
        let mut tags = Vec::new();
        if p.terminates {
            tags.push("the body may end the loop");
        }
        if p.speculate {
            tags.push("speculate");
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!(" ({})", tags.join(", "))
        };
        out += &format!("  prefetch: {}{tags}\n", p.path);
    }
    for n in &f.nodes {
        let what = match n.kind.as_str() {
            "call" => format!("call {}", n.callee),
            k => k.to_string(),
        };
        let effects = if n.effects.is_empty() {
            String::new()
        } else {
            format!("  [{}]", n.effects.join("; "))
        };
        let guards: Vec<String> = n
            .guards
            .iter()
            .map(|g| {
                let b = f.branches.iter().find(|b| b.block == g.branch);
                match b {
                    Some(b) if !b.uses.is_empty() => format!(
                        "b{} (line {}, {} on {}) = {}",
                        g.branch,
                        b.line,
                        b.test,
                        blocks(&b.uses),
                        g.label
                    ),
                    Some(b) => format!("b{} (line {}, {}) = {}", g.branch, b.line, b.test, g.label),
                    None => format!("b{} = {}", g.branch, g.label),
                }
            })
            .collect();
        let guards = if guards.is_empty() {
            String::new()
        } else {
            format!("  if {}", guards.join(" and "))
        };
        let uses = if n.uses.is_empty() {
            String::new()
        } else {
            format!("  uses {}", blocks(&n.uses))
        };
        out += &format!(
            "  b{:<3} line {:<4} {what}{effects}{uses}{guards}\n",
            n.block, n.line
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(edges: &[&[(usize, &str)]]) -> Cfg {
        Cfg {
            succ: edges
                .iter()
                .map(|s| s.iter().map(|(b, l)| (*b, l.to_string())).collect())
                .collect(),
        }
    }

    #[test]
    fn diamond() {
        // 0: if c { 1 } else { 2 }; 3
        let g = cfg(&[&[(1, "true"), (2, "false")], &[(3, "")], &[(3, "")], &[]]);
        assert_eq!(post_dominators(&g), [Some(3), Some(3), Some(3), None]);
        let cd = control_dependence(&g);
        assert_eq!(
            cd[1],
            [Guard {
                branch: 0,
                label: "true".into()
            }]
        );
        assert_eq!(
            cd[2],
            [Guard {
                branch: 0,
                label: "false".into()
            }]
        );
        assert!(cd[0].is_empty() && cd[3].is_empty());
    }

    #[test]
    fn early_return() {
        // 0: let x = f()?  (Err -> 1: return); 2: g(); 3: return
        let g = cfg(&[&[(2, "Ok"), (1, "Err")], &[], &[(3, "")], &[]]);
        let cd = control_dependence(&g);
        // `g()` runs only when `f()` succeeded.
        assert_eq!(
            cd[2],
            [Guard {
                branch: 0,
                label: "Ok".into()
            }]
        );
        assert_eq!(
            cd[3],
            [Guard {
                branch: 0,
                label: "Ok".into()
            }]
        );
        assert_eq!(
            cd[1],
            [Guard {
                branch: 0,
                label: "Err".into()
            }]
        );
    }

    #[test]
    fn loop_body() {
        // 0 -> 1: while c { 2 -> 1 }; 3
        let g = cfg(&[&[(1, "")], &[(2, "true"), (3, "false")], &[(1, "")], &[]]);
        let cd = control_dependence(&g);
        assert_eq!(
            cd[2],
            [Guard {
                branch: 1,
                label: "true".into()
            }]
        );
        // The loop head runs again only when the body ran: it depends on its own branch.
        assert_eq!(
            cd[1],
            [Guard {
                branch: 1,
                label: "true".into()
            }]
        );
        assert!(cd[3].is_empty());
    }

    #[test]
    fn renders_joins_and_prefetches() {
        let f = FnPdg {
            path: "f::{closure#0}".into(),
            joins: vec![PdgJoin {
                level: 1,
                chains: vec![
                    PdgChain {
                        path: "f::{closure#0}::{closure#0}".into(),
                        first: 0,
                        last: 2,
                        exits: true,
                        speculate: false,
                        independent: false,
                        sync: false,
                    },
                    PdgChain {
                        path: "f::{closure#0}::{closure#1}".into(),
                        first: 1,
                        last: 1,
                        exits: false,
                        speculate: true,
                        independent: false,
                        sync: false,
                    },
                ],
            }],
            prefetches: vec![PdgPrefetch {
                path: "f::{closure#0}::{closure#2}".into(),
                terminates: true,
                speculate: false,
            }],
            ..Default::default()
        };
        assert_eq!(
            render(&f),
            "f::{closure#0}\n  join 1: f::{closure#0}::{closure#0} (statements 0-2, may exit) || \
             f::{closure#0}::{closure#1} (statement 1, speculate)\n  prefetch: \
             f::{closure#0}::{closure#2} (the body may end the loop)\n"
        );
    }

    #[test]
    fn unreachable_exit_is_tolerated() {
        // 0: loop {} with no exit; 1 unreachable from 0.
        let g = cfg(&[&[(0, "")], &[]]);
        assert_eq!(post_dominators(&g), [None, None]);
        assert!(control_dependence(&g).iter().all(Vec::is_empty));
    }
}
