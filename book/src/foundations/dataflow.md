# Dependencies and dataflow

psync's whole job is to answer one question: *which statements may run at the same time?* The
compiler-science answer has two parts. **Dependence analysis** decides which pairs of
statements must keep their order. **Scheduling** turns the remaining freedom into concurrency.
This chapter builds both from scratch. It ends with **dataflow analysis**, the general method
compilers use for questions about programs with loops.

## Reads, writes and three kinds of dependence

Describe each statement by what it **writes** (binds or mutates) and what it **reads**. Two
statements `a` (earlier) and `b` (later) must keep their order if one of these holds:

| Name | Pattern | Example | Why order matters |
|---|---|---|---|
| **true dependence** (read after write) | `b` reads what `a` writes | `let u = f(); g(u);` | `b` needs `a`'s value |
| **anti-dependence** (write after read) | `b` writes what `a` reads | `g(x); x = 5;` | `a` must see the old value |
| **output dependence** (write after write) | both write it | `x = 1; x = 2;` | the final value must be `b`'s |

If none holds, the two statements **commute**: running them in either order, or interleaved,
gives the same result. That fact is the first theorem in `verify/Schedule.lean`. Two statements
that only read the same thing have no dependence at all. Reads never conflict with reads.

psync's `depends` function (`psync-graph/src/schedule.rs`) asks these questions in a fixed order
and reports the first that applies:

| Reason psync prints | Kind |
|---|---|
| ``reads `x` `` | true dependence on a binding |
| ``rebinds `x` `` | output dependence: both bind `x` |
| ``shadows `x` `` | anti-dependence: `b` rebinds a name `a` reads |
| ``writes `p` `` | a true, anti or output dependence on a place such as `self.groups` |
| `effect order` | two ordinary statements with possible side effects (`out.push(x)`, `total += n`), which psync never reorders relative to each other |
| effect conflicts, `may exit before` | effects on files and services, and exits, the [next chapter](./effects.md) |

Places are compared by **prefix**. A write to `self.groups` overlaps a read of `self` or of
`self.groups.len`, but not of `self.client`. That keeps the analysis useful for methods on
`self`, where nearly everything starts with the same root.

## From edges to a schedule

Draw a **dependence graph**: one node per statement, and an edge `a → b` for each dependence.
Edges always point forward in program order, so the graph has no cycles. It is a **DAG**
(directed acyclic graph). Any order of the nodes that respects every edge is a **topological
order**, and running the program in any topological order gives the same result.

Concurrency comes from nodes with no path between them. psync groups them in two steps.

1. **Chains.** If `u → v` is the only edge into `v` and the only edge out of `u`, nothing else
   can happen between them that matters. Fuse them into one **chain** and run it as one task.
2. **Levels.** A chain's level is one past the deepest chain it waits for. All chains of a level
   run concurrently. The next level starts when they are all done.

The example runs this on the function `hoisted` from [Basic concepts](../guide/concepts.md),
after psync has moved its two awaits into their own bindings (`h2`, `s3`). It runs it on the
`chains` test too:

```sh
cargo run -p psync-graph --example dataflow
```

```text
Part A: the dependency graph of `hoisted`
  n0: let mut lens = Vec::new();
  n1: let h2 = api.len(&names[0]).await;  [await]
  n2: lens.push(h2);
  n3: let s3 = api.kind(&names[1]).await;  [await]
  n4: match s3 { .. => lens.push(..) }
  n0 -> n2 (reads `lens`)
  n1 -> n2 (reads `h2`)
  n0 -> n4 (reads `lens`)
  n2 -> n4 (writes `lens`)
  n3 -> n4 (reads `s3`)
  chain 0: n0
  chain 1: n1
  chain 2: n2
  chain 3: n3
  chain 4: n4
  level 0: n0 || n1 || n3
  level 1: n2
  level 2: n4
```

```text
Part A: the dependency graph of `chains`
  n0 -> n2 (reads `a1`)
  n1 -> n3 (reads `a2`)
  chain 0: n0 -> n2
  chain 1: n1 -> n3
  level 0: n0->n2 || n1->n3
```

For `hoisted`, every chain is a single node, and the graph and schedule are the ones psync itself
prints for that function. The two requests,
`n1` and `n3`, start together. The two `push`es keep their order because of the edge
`n2 → n4 (writes `lens`)`.

### Why chains, not just levels

Levels alone wait for the slowest member of the level. With `a1 → b1` and `a2 → b2`, where
`a1` takes 1 unit, `a2` 2, `b1` 2 and `b2` 1:

```text
levels only:    a1 ─┐          ┌─ b1 ──┐
                a2 ─┴──────────┴─ b2 ──┴──►   2 + 2 = 4 units
chains:         a1 ── b1 ──────┐
                a2 ──── b2 ────┴──►           max(1+2, 2+1) = 3 units
```

The length of the longest path through the DAG, weighted by time, is the **critical path**. No
schedule can beat it. Chains get much closer to it than levels do, and the `chains` test in
`tests/graph.rs` checks the 3-unit time. psync does not know how long anything takes, so it
cannot always find the best schedule. Chains are a cheap rule that never makes things worse.

## Loops: dependences across iterations

Inside a loop body, the same analysis applies per iteration. The new question is whether
iteration `k+1` depends on iteration `k`. That is a **loop-carried dependence**:

```rust,ignore
for key in keys {
    let meta = store.head(key).await?;   // reads only per-iteration data
    total += meta.size;                  // writes `total`, which outlives the iteration
}
```

`total` is carried: each iteration reads what the previous one wrote. But the awaited prefix,
`store.head(key)`, reads only `key` and `store`. It does not depend on any earlier iteration.
So psync runs the prefixes of up to `bound` iterations ahead (an option of the attribute,
16 by default), and then runs the rest of each body
(`total += ..`) in order. This is **software pipelining**. When the carried state is in the
middle of the body, psync first splits the loop into two loops, which is **loop fission**. Both
are described in [Loop rewriting](../internals/loops.md).

## Dataflow analysis: questions about programs with cycles

A region's dependence graph (one block's run of movable statements between barriers) is acyclic, so one pass in program order computes every level. The
control-flow graphs of the [previous chapter](./rustc.md) have cycles wherever there is a loop,
and many questions about them cannot be answered in one pass. **Dataflow analysis** is the
general method for those questions.

The classic example is **liveness**. A variable is *live* at a point if its current value may
still be read later. The borrow checker uses liveness, and so does register allocation. For
each block, with `uses` the variables it reads before writing and `defs` those it writes:

```text
live_out(b) = ∪ live_in(s)  for every successor s of b
live_in(b)  = uses(b) ∪ (live_out(b) − defs(b))
```

In a loop, `live_in(bb1)` depends on `live_in(bb2)`, which depends on `live_in(bb1)`. A circular
definition is solved by **iterating to a fixed point**. Start with every set empty, apply the
equations to every block, and repeat until nothing changes:

```text
Part B: liveness, visiting blocks backward, fixed point after 2 rounds
  bb0: i = 0; s = 0          live on entry: {"v"}
  bb1: if i < v.len()        live on entry: {"i", "s", "v"}
  bb2: s += v[i]; i += 1     live on entry: {"i", "s", "v"}
  bb3: return s              live on entry: {"s"}
```

Why does this terminate, and why is the answer right?

- **Monotone.** Each equation can only grow a set when its inputs grow. The sets start empty, so
  they only ever grow.
- **Finite.** There are finitely many variables, so a set can grow only finitely often.
- **Least.** Starting from empty and growing only as forced gives the *smallest* solution of the
  equations, the **least fixed point**. Any variable in it really can be read later along some
  path.

That reasoning, with sets replaced by any **lattice** (a set of facts ordered by "knows more"),
is the general theory, and rustc's `rustc_mir_dataflow` crate implements it for MIR. You will
meet it again in the [next chapter](./effects.md). Effect summaries of recursive functions are a
fixed point over the call graph, in the same way.

### Flow-sensitive and flow-insensitive

Liveness is **flow-sensitive**: the answer differs from point to point in the function. Many
analyses do not need that. psync's driver summarizes a function by taking the union of the
effects of *every* call in *every* basic block, ignoring which blocks are reachable from which.
That is **flow-insensitive**. It is simpler and faster. It is also sound for psync's purpose
("what might this function do?"), at the cost of counting effects on paths that never run
together.

## Exercises

1. In `psync-graph/examples/dataflow.rs`, add a statement `let n = h2 + 1;` to `hoisted`, between
   `n2` and `n3`. Each statement is an `Op` listing what it binds (`defs`), reads (`uses`) and
   mutates (`writes`), so this one is
   `Op { text: "let n = h2 + 1;", defs: set(&["n"]), uses: set(&["h2"]), writes: set(&[]), awaits: false }`.
   Which edges appear? Which chains? Does the number of levels change?
2. Two statements `x.push(1);` and `x.push(2);` on a `let mut x = Vec::new()`. Which kind of
   dependence is this? What would go wrong if psync ran them concurrently? (There are two
   answers: one from the borrow checker, one about the result.)
3. In the liveness example, why is `s` live on entry to `bb1` but not on entry to `bb0`?
4. Run the example with `-- --forward`, which visits the blocks first to last instead of last to
   first. Does the answer change? Does the number of rounds?
5. The chain rule fuses `u → v` only if `u` has exactly one successor. Give a three-node example
   where fusing `u → v` anyway, even though `u` also has an edge to `w`, would delay `w`.

<details>
<summary>Answers</summary>

1. One new edge, `n1 → n` (reads `h2`). Now `n1` has two successors, `n2` and `n`, so neither
   edge out of `n1` fuses. `n` lands in level 1 next to `n2`, and there are still 3 levels.
   (psync's real `plan` fuses in one more case: when `u` does not await and everything else
   after `u` waits for `v` anyway. `n1` awaits, so that case does not apply.)
2. An output dependence on `x`, together with a true and an anti-dependence, since `push` both
   reads and writes `x`. Running them concurrently needs two `&mut x` at once, which the borrow
   checker rejects. Even with a lock, the order of the elements could change.
3. `bb0` *defines* `s` (`s = 0`) before anything reads it, so the value `s` had before `bb0` is
   never used. In `bb1`, the current `s` will be read later, in `bb2` or `bb3`.
4. The answer is the same: the least fixed point does not depend on the visiting order. The
   number of rounds grows from 2 to 3. Liveness flows backwards, from uses to earlier points, so
   visiting blocks in reverse lets one round carry information further.
5. `let k = key(); let r = api.get(&k).await; let m = slow_other(&k).await;`. Here `u = k` has
   edges to `v = r` and `w = m`. Fusing `k → r` into one chain means `m` waits for the whole
   chain, including the request `r`, when it only needed `k`. psync allows the fusion only when
   `u` does not await and `w` would wait for `v` anyway.

</details>
