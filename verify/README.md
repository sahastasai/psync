# Machine-checked: why psync's schedules are safe

`Schedule.lean` models a program as operations on a state, each with a footprint (what it may read
and write), and proves in Lean 4 (core library only, no Mathlib):

| Theorem | Statement |
|---|---|
| `commute` | two independent operations (neither writes what the other reads or writes) commute |
| `interleave_correct` | any interleaving of two pairwise independent chains, each kept in order, equals running one chain and then the other |
| `level_correct` | the same for any number of chains: what a level's join does |
| `levels_correct` | a schedule of levels, each run concurrently in any interleaving, equals running every chain in order |
| `reorders_correct` | an order that moves operations earlier only past independent ones equals the program as written |
| `speculation_harmless` | operations that write nothing leave the state unchanged, so the reads psync may run past an exit change nothing |

Together: the concurrent execution of psync's schedule ends in the state the sequential program
ends in, provided the footprints are right and psync draws an edge between every pair of
operations that are not independent. Those are the obligations of the access and effect analyses,
which `cargo psync check` verifies against the compiled callees.

Not modeled: early-exit replay, and the generated Rust (rustc type- and borrow-checks it).

```sh
lean verify/Schedule.lean      # Lean 4.22; prints nothing when every proof checks
```

The proofs use only Lean's standard axioms (`propext`, `Classical.choice`, `Quot.sound`) and
contain no `sorry`.
