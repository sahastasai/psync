# Glossary

**Access path.** A variable followed by fields, such as `self.groups`. Reads and writes are
tracked per path. Two paths overlap when one is a prefix of the other.

**AST.** Abstract syntax tree: the program as a tree that follows Rust's grammar. Macros see it through `syn`. See [Syntax trees](../foundations/syntax-trees.md).

**Barrier.** A statement that stays exactly in place and splits its block into regions. Examples are
a held lock, a sleep, a clock read, `#[psync::seq]`, an exit hidden in a macro, and the block's
value.

**Basic block.** A run of MIR statements with one entry and one exit, ended by a *terminator* (`goto`, `switchInt`, `Call`, `return`, ...). See [Inside rustc](../foundations/rustc.md).

**Bound.** The maximum number of a loop's prefixes in flight at once (`bound = N`, default 16).
Nested loops multiply their bounds.

**Certificate.** What psync claims about one region: each statement's places and the levels of chains it chose. A Lean checker, proven sound, accepts or rejects it. See [Certified schedules](../safety/certificates.md).

**Chain.** Nodes fused into one sequential future, because each one waits only for the one before
it. A chain starts its next node as soon as its previous one finishes, whatever the other chains
are doing.

**Class-hierarchy analysis (CHA).** Resolving a call through `dyn` or a generic to every implementation of the trait method in the program. psync does it at link time, over the whole build. See [Dynamic dispatch](../foundations/dispatch.md).

**Closed world.** The assumption that the analysed build contains every implementation that can run. Facts hold for the build they were computed in.

**Commutativity specification.** Which requests to a server commute: in either order, the server ends in the same state and each gets the same response. psync's schedules are proven correct relative to one; psync's built-in rule is one. See [the outside world](../safety/world.md).

**Conflict.** Two effects that must keep their order: one writes, they are in the same domain, and their targets may be the same.

**Coroutine.** The state machine an `async fn` or block compiles to. Each `.await` is a point where it can suspend.

**`DefId`, `DefPathHash`.** rustc's name for a definition within one compilation, and a stable hash of its path that is the same in every compilation. Summary files use the hash.

**Edge.** `a -> b`: `b` must wait for `a`. The reasons are data flow, rebinding, shadowing, a
conflicting write, or effect order.

**Effect.** An ordinary statement that is not a binding of an awaited or pure value. Effects never
change order relative to each other. (The [side-effects chapter](../safety/effects.md) also uses
the word for file-system, network and process operations.)

**Exit slot.** A variable in which a chain records a `?` residual, a `return` value, or which
`break`/`continue` it took. The slot is replayed after the join, in program order.

**Fission.** Splitting a loop into a plain first loop, which updates carried state for every item,
and a prefetched second loop.

**Fixed point.** The solution of equations that define a set in terms of itself (liveness, summaries of recursive functions), found by iterating until nothing changes.

**Fresh binding.** A `#[psync::fresh]` `let` whose initializer every concurrent chain evaluates
again.

**Guard.** An `if` around awaited work in a loop or block. Its work runs ahead only when the
condition holds. An *inverted* guard is an early `continue`/`break`/`return` whose negation guards
the awaited work after it.

**HIR, THIR, MIR.** rustc's forms after macro expansion: the desugared program (HIR), with every expression typed and every method call resolved (THIR), and as a control-flow graph of simple statements (MIR). `psync-driver` reads MIR.

**Hoisting.** Moving an await out of a larger statement (scrutinee, argument, receiver, branch,
tail) into its own binding, while keeping evaluation order.

**Labelled transition system.** A model of a server: each request (the label) takes its state to a new state and a response. Reads that change state are ordinary transitions.

**Level.** A set of chains joined together. Each level waits for the one before it.

**Node.** A statement psync may move: a binding, an awaited statement, or an effect.

**Opaque.** A summary with a callee the driver could not see through (a function pointer, a `dyn` trait of another crate). The macro then keeps its name-based guess as well.

**Prefetch.** Running the awaited prefix of every loop iteration ahead, at most `bound` at a time,
before running the rest of each body in order.

**Prefix.** The leading statements of a loop body that touch only per-iteration data and may run
ahead.

**Region.** A run of nodes between barriers. Each region gets its own graph and schedule.

**Speculation.** Work that runs although sequential code would have skipped it after an earlier
exit. It is allowed for awaited operations, and limited by the side-effects analysis.

**Strongly connected component (SCC).** A maximal set of functions that can all reach each other through calls. The driver solves each one as a fixed point.

**Summary.** Every effect a call to a function may have, through every function it calls.

**Token tree.** What a procedural macro receives: identifiers, punctuation, literals, and bracketed groups of further trees. See [Tokens](../foundations/tokens.md).
