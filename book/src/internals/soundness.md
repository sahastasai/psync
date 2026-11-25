# What is checked, what is assumed

psync's analysis is token-level. It sees no types, no name resolution and no function bodies other
than the one it rewrites. This chapter states exactly which properties come from the compiler,
which from psync's rules, and which you assume when you add the attribute.

## Checked by rustc

The generated code is ordinary Rust, compiled with the rest of your crate.

- **Memory safety and data-race freedom.** Two concurrent branches that need the same `&mut` fail
  to compile (E0499/E0502). A missed write dependency through a unique borrow cannot race. It is a
  compile error.
- **Types.** Every hoisted binding, handed-back tuple and replayed exit is type-checked. Because
  `Rethrow` goes through your own `?`, the error conversion is the original one.
- **`Send`/`Sync`.** If your function's future had to be `Send`, the rewritten one does too, or it
  does not compile.
- **Lifetimes.** Borrows made by branches end at the join, which rustc verifies.

## Guaranteed by psync's rules

These hold as long as the token analysis sees the dependency:

- **Def-use order.** A statement that uses a binding runs after the statement that made it.
- **Write conflicts by path.** Two statements touching overlapping paths, one of them writing,
  keep their order. That covers assignment, `&mut`, a mutating method on a `mut` binding or `&mut`
  handle, and an ordered method (`send`, `write`, `next`, ...) on any receiver.
- **Effect order.** Ordinary statements never pass each other.
- **Exits.** `?`, `return`, `break` and `continue` are performed in program order, each only after
  every statement before it has run. The function exits at the same place, with the same value.
  `replay_correct` in `verify/Schedule.lean` proves this rule.
- **Fixed positions.** Locks and permits held past their statement, sleeps, clock reads and
  `#[psync::seq]` statements never move, and nothing moves across them.
- **Loops.** A prefetched prefix touches only per-iteration data, and the rest of the body runs in
  order. Fission is applied only when its halves share no carried state.

## Assumed by you

When you add `#[psync::graph]`, you assert:

1. **Unclassified calls tolerate speculation.** Awaited calls that psync recognizes as writes never
   run before an earlier exit (see [Side effects](../safety/effects.md)). Calls it cannot classify
   are assumed to read, and may run when sequential code would have stopped at an earlier exit. In
   loops, every item's prefix runs, even after an earlier item fails. `strict` removes this
   assumption.
2. **Different handles are different resources.** Two calls through different receivers
   (`self.cache`, `self.db`) may run at the same time. That includes two handles to the same
   server, and interior mutability behind `&self`. A `RefCell` borrowed twice at once panics. It
   does not race.
3. **Names mean what they say.** `get`, `list` and `head` read; `put`, `delete` and `create` write;
   on `mut` receivers, read-only prefixes like `get` and `is_` are taken as reads.
4. **Loop items name different objects.** A write target computed from the loop item is assumed to
   differ between iterations. For files whose path can be evaluated ahead this is checked at run
   time, and with a duplicate the loop runs as written ([Loop rewriting](./loops.md#checking-per-item-files)).
5. **Hints are true.** `#[psync::independent]`, `#[psync::fresh]` and `#[psync::speculate]` turn
   checks off.

## Machine-checked

`verify/Schedule.lean` proves the scheduling argument in Lean 4. It models a program as operations
with footprints (what each may read and write) and shows:

- independent operations commute;
- any interleaving of a level's pairwise-independent chains equals running the chains one after
  another, and so does a whole schedule of levels;
- an order that moves operations earlier only past independent ones equals the program as written;
- operations that write nothing, which are the only ones psync may run past an exit, change
  nothing;
- recording every outcome and replaying the first exit in program order, after everything has
  run, gives the sequential state and exit (`replay_correct`).

So the concurrent execution ends in the state of the sequential program, *provided* two things
hold. The footprints must be right, and psync must draw an edge between every two operations that
are not independent. Those are the analyses' obligations. Tokens cannot discharge them alone,
which is why [`cargo psync check`](../safety/check.md) verifies them against the compiled callees.

The generated Rust is not modeled. rustc checks it.

## How this compares with dynamic languages

Tools that reorder awaits in JavaScript rely on a whole-program side-effect analysis. They state
that it is unsound (aliasing, dynamic calls). Then they validate the rewrites with the project's
tests. psync has less information per statement, because it sees tokens rather than a call graph.
But it gets a guarantee those tools cannot have: **every missed `&mut` dependency is caught by the
borrow checker before the program runs.** The remaining assumptions are the ones listed above, and
each can be turned off locally with a hint.

## Tested behaviour

`tests/graph.rs` runs generated code on psync's executor, with I/O simulated by poll counting. It
checks results, peak in-flight counts and poll counts against the sequential schedule:

- trees and chains;
- first error wins, `Option` `?`, `return` order;
- shadowing and `mut`, `&mut self`;
- loop prefetch with a bound, loop-carried state, per-iteration exits;
- guards with field sensitivity, guard inversion, fission and deferred exits;
- `while` with `independent`, nested loops;
- locks (kept and scoped), clock reads;
- `seq`, `fresh`, `independent`;
- `Send` over trait objects.
