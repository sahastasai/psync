# Glossary

**Access path.** A variable followed by fields, such as `self.groups`. Reads and writes are
tracked per path. Two paths overlap when one is a prefix of the other.

**Barrier.** A statement that stays exactly in place and splits its block into regions. Examples are
a held lock, a sleep, a clock read, `#[psync::seq]`, an exit hidden in a macro, and the block's
value.

**Bound.** The maximum number of a loop's prefixes in flight at once (`bound = N`, default 16).
Nested loops multiply their bounds.

**Chain.** Nodes fused into one sequential future, because each one waits only for the one before
it. A chain starts its next node as soon as its previous one finishes, whatever the other chains
are doing.

**Edge.** `a -> b`: `b` must wait for `a`. The reasons are data flow, rebinding, shadowing, a
conflicting write, or effect order.

**Effect.** An ordinary statement that is not a binding of an awaited or pure value. Effects never
change order relative to each other. (The [side-effects chapter](../safety/effects.md) also uses
the word for file-system, network and process operations.)

**Exit slot.** A variable in which a chain records a `?` residual, a `return` value, or which
`break`/`continue` it took. The slot is replayed after the join, in program order.

**Fission.** Splitting a loop into a plain first loop, which updates carried state for every item,
and a prefetched second loop.

**Fresh binding.** A `#[psync::fresh]` `let` whose initializer every concurrent chain evaluates
again.

**Guard.** An `if` around awaited work in a loop or block. Its work runs ahead only when the
condition holds. An *inverted* guard is an early `continue`/`break`/`return` whose negation guards
the awaited work after it.

**Hoisting.** Moving an await out of a larger statement (scrutinee, argument, receiver, branch,
tail) into its own binding, while keeping evaluation order.

**Level.** A set of chains joined together. Each level waits for the one before it.

**Node.** A statement psync may move: a binding, an awaited statement, or an effect.

**Prefetch.** Running the awaited prefix of every loop iteration ahead, at most `bound` at a time,
before running the rest of each body in order.

**Prefix.** The leading statements of a loop body that touch only per-iteration data and may run
ahead.

**Region.** A run of nodes between barriers. Each region gets its own graph and schedule.

**Speculation.** Work that runs although sequential code would have skipped it after an earlier
exit. It is allowed for awaited operations, and limited by the side-effects analysis.
