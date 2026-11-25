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
}

/// A branch: where it is and what it tests.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PdgBranch {
    pub block: usize,
    pub line: usize,
    pub test: String,
}

/// The dependence graph of one body (a function, closure or async block).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FnPdg {
    pub path: String,
    pub nodes: Vec<PdgNode>,
    pub branches: Vec<PdgBranch>,
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
    let mut out = format!("{}\n", f.path);
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
        out += &format!(
            "  b{:<3} line {:<4} {what}{effects}{guards}\n",
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
    fn unreachable_exit_is_tolerated() {
        // 0: loop {} with no exit; 1 unreachable from 0.
        let g = cfg(&[&[(0, "")], &[]]);
        assert_eq!(post_dominators(&g), [None, None]);
        assert!(control_dependence(&g).iter().all(Vec::is_empty));
    }
}
