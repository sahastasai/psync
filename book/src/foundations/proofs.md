# Proofs

Tests show that psync is right on the programs that were tested. A **proof** shows it is right
on *every* program, given stated assumptions. `verify/Schedule.lean` is such a proof for the core
idea: running psync's schedule concurrently ends in the same state as running the program in
order. This chapter teaches enough Lean to read that file, check it, and extend it.

## What a proof assistant does

A **proof assistant** is a programming language in which a theorem is a *type*, and a proof is a
*value of that type*. If the program type-checks, the proof is correct. You trust the small checker
at the core of the tool, not the person who wrote the proof. Lean 4 is one such tool, and it is
also an ordinary functional language, so definitions, programs and proofs live in one file.

Install it with `elan`, then check a file:

```sh
lean verify/Tutorial.lean      # prints nothing when every proof checks
lean verify/Schedule.lean
```

`Tutorial.lean` holds every example in this chapter, and the solutions to its exercises.

## First steps

```lean
def double (n : Nat) : Nat := n + n

theorem double_21 : double 21 = 42 := rfl
```

`def` defines a function. `theorem name : statement := proof` states something and proves it.
`double 21 = 42` is a type, and `rfl` (reflexivity) is a value of it when both sides compute to
the same thing. To prove something for *every* `n`, computation is not enough:

```lean
theorem double_eq (n : Nat) : double n = 2 * n := by
  unfold double
  omega
```

`by` switches to **tactic mode**. Instead of writing the proof term yourself, you give commands
that transform the goal until nothing is left. The goal starts as `double n = 2 * n`. After
`unfold double` it is `n + n = 2 * n`. `omega`, a decision procedure for linear arithmetic, then
closes it. In an editor with Lean support you can see the goal after each line, which is the
normal way to write proofs.

## Programs as lists of steps

`Schedule.lean` models a program as a list of operations, each a function from state to state.
Here is the same idea with numbers:

```lean
def applyAll : List (Nat → Nat) → Nat → Nat
  | [], s => s
  | f :: fs, s => applyAll fs (f s)
```

`applyAll [f, g] s` is `g (f s)`: run `f`, then `g`. Now prove that running `xs ++ ys` means
running `xs` and then `ys`:

```lean
theorem applyAll_append (xs ys : List (Nat → Nat)) (s : Nat) :
    applyAll (xs ++ ys) s = applyAll ys (applyAll xs s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih => exact ih (x s)
```

This is **induction** on the list, the proof version of a recursive function. There are two
cases. For `[]`, both sides compute to `applyAll ys s`, so `rfl`. For `x :: xs` we may assume
the statement for `xs` (the **induction hypothesis** `ih`). By the second equation of `applyAll`,
the goal is `applyAll (xs ++ ys) (x s) = applyAll ys (applyAll xs (x s))`, which is `ih` with the
state `x s`. `generalizing s` makes `ih`
hold for *every* starting state, which is needed because the state changes from one step to the
next. `Schedule.lean` has exactly this theorem, `runAll_append`.

## Commutation

The heart of psync is that independent operations can be swapped. With two functions:

```lean
theorem swap2 (f g : Nat → Nat) (h : ∀ s, f (g s) = g (f s)) (s : Nat) :
    applyAll [f, g] s = applyAll [g, f] s := by
  show g (f s) = f (g s)
  rw [h]
```

`h` is a **hypothesis**: the theorem only holds for `f` and `g` that commute. `show` restates the
goal in a form Lean can see is equal to it. `rw [h]` (rewrite) replaces `f (g s)` with
`g (f s)` using `h`, which closes the goal.

What makes two operations commute? In psync's model, state is a function from **locations**
(variables, fields, files) to values:

```lean
abbrev State := String → Nat

def set (x : String) (v : Nat) (s : State) : State := fun y => if y = x then v else s y

theorem set_comm (x y : String) (a b : Nat) (hxy : x ≠ y) (s : State) :
    set x a (set y b s) = set y b (set x a s) := by
  funext z
  unfold set
  by_cases hx : z = x <;> by_cases hy : z = y <;> simp_all
```

Writes to different locations commute. `funext z` says that two functions are equal if they agree
at every `z`. `by_cases` splits into the cases `z = x` or not, and `z = y` or not. `<;>` applies
the next tactic to every case, and `simp_all` simplifies each case, using `hxy` to rule out
`z = x ∧ z = y`.

## Four more constructs

`Schedule.lean` uses four things the examples so far have not. All four are in section 4 of
`Tutorial.lean`.

- **Sets as predicates.** A set of locations is written as a function to `Prop`, the type of
  propositions: `R : Loc → Prop`, where `R l` is the statement "`l` is in `R`". The tutorial's
  `Small : Nat → Prop := fun n => n < 10` is the set of numbers below 10.
- **Structures.** `structure` declares a record type. Its fields can be *proofs* as well as data,
  so a value of the type comes with guarantees attached:

  ```lean
  structure Bump where
    step : Nat → Nat
    grows : ∀ n, n ≤ step n          -- every Bump comes with this proof
  ```

- **Implicit arguments and `∧`.** Arguments in braces, `{a b : Nat}`, are filled in by Lean from
  the other arguments. A proof `h : P ∧ Q` is a pair: `h.1 : P` and `h.2 : Q`.
- **Inductive predicates.** `inductive` defines a proposition by rules, and a proof is a
  derivation built from them:

  ```lean
  inductive Even : Nat → Prop
    | zero : Even 0
    | step {n : Nat} : Even n → Even (n + 2)

  theorem four_even : Even 4 := Even.step (Even.step Even.zero)
  ```

  To prove something about every `Even n`, do induction over the rules, one case per rule.

Finally, the tutorial's `State` is fixed to `String → Nat`. `Schedule.lean` writes
`State (Loc Val : Type) := Loc → Val`, the same idea for any type of locations and values, which
is why its definitions carry the parameters `Loc` and `Val` around.

## Reading `Schedule.lean`

Here is the file's structure.

**An operation with a footprint.**

```lean
structure Op (Loc Val : Type) where
  run : State Loc Val → State Loc Val
  R : Loc → Prop                     -- the locations it may read
  W : Loc → Prop                     -- the locations it may write
  frame : ∀ s l, ¬ W l → run s l = s l
  local_dep : ∀ s s', (∀ l, R l → s l = s' l) → ∀ l, W l → run s l = run s' l
```

A value of `Op` packages a function with two *proofs* about it. `frame` says it changes only what
it writes, and `local_dep` says what it writes depends only on what it reads. These are exactly the
promises psync's access and effect analyses must keep. `R` and `W` are the read and write sets
from the [dataflow chapter](./dataflow.md), with files and services as locations too.

**Independence is "no edge".**

```lean
def Independent (a b : Op Loc Val) : Prop :=
  (∀ l, a.W l → ¬ b.R l ∧ ¬ b.W l) ∧ (∀ l, b.W l → ¬ a.R l)
```

Neither writes what the other reads or writes. This is the negation of the three kinds of
dependence, and psync draws an edge exactly when it does not hold.

**The theorems build on each other.**

| Theorem | Says | Proof idea |
|---|---|---|
| `commute` | independent operations commute | look at one location `l`: at most one of them writes it, and `frame` and `local_dep` do the rest |
| `commute_list` | an operation independent of a whole list commutes with the list | induction on the list, using `commute` |
| `interleave_correct` | any interleaving of two independent chains equals one chain, then the other | induction on the interleaving, `Interleave`, using `commute_list` |
| `level_correct` | the same for any number of chains: one level's join | induction on the chains |
| `levels_correct` | a whole schedule of levels | induction on the levels |
| `reorders_correct` | moving operations earlier only past independent ones changes nothing | induction on the reordering |
| `speculation_harmless` | operations that write nothing leave the state unchanged | `frame`, by induction |
| `replay_correct` | running every statement and replaying the first recorded exit gives the sequential state and exit, if nothing after the first exit writes | `speculation_harmless` for the rest of the list, by induction |
| `recording_slot` | a statement that records its outcome is an ordinary `Op` with that outcome in its slot | unfolding |

`Interleave a b m` is an **inductive predicate**. It defines "`m` is some interleaving of `a` and
`b`" by three rules: both empty, take the next element from `a`, or take it from `b`. Proving
something for *every* interleaving is then an induction over those rules. Each rule is one
possible next step of the executor. This is how the proof covers every way the executor might
poll the chains. Two more definitions tie it to psync's schedule:

```lean
def PairwiseIndependent : List (List (Op Loc Val)) → Prop        -- the chains of one level
  | [] => True
  | c :: cs => (∀ c' ∈ cs, ∀ x ∈ c, ∀ y ∈ c', Independent x y) ∧ PairwiseIndependent cs

inductive Runs : List (List (List (Op Loc Val))) → List (List (Op Loc Val)) → Prop
  | nil : Runs [] []
  | cons {cs m levels ms} :
      PairwiseIndependent cs → InterleaveAll cs m → Runs levels ms → Runs (cs :: levels) (m :: ms)
```

`Runs levels ms` says that `ms` is one possible execution of the schedule `levels`. Each level is a
list of chains that must be pairwise independent, and its execution `m` is some interleaving of
them (`InterleaveAll`, built from `Interleave`).

## What is proven, what is assumed

Read a theorem's hypotheses as the list of things everything else must guarantee. For
`levels_correct`:

- **Footprints are correct.** Each `Op`'s `frame` and `local_dep` hold. In psync these are the
  obligations of the access analysis (variables and fields) and the effect analysis (files and
  services). `cargo psync check` checks the effect part against the compiled callees.
- **Chains in a level are pairwise independent.** That is psync's edge rule.

The last section, *Early exits*, adds statements that may exit. An `XOp` is an `Op` with an
`exit` function, whose outcome depends only on what the statement reads. `seqX` runs a list
sequentially and stops at the first exit. `outcomes` runs every statement and records each
outcome, as psync's exit slots do, and `firstExit` replays the first one. `replay_correct` proves
that the two agree, in state and in exit, when no statement after the first exit writes. That is
the condition psync's speculation rule enforces. `XOp.recording` turns an `XOp` into an ordinary
`Op` that also writes its outcome into its own slot, so the scheduling theorems apply to
statements that record their exits.

Note what the replay needs: *every* statement before an exit has run. psync once broke this, by
replaying an exit before an earlier statement in a later level had run; see
[Early exits](../guide/exits.md).

Not modeled: the generated Rust. rustc type- and borrow-checks it.

## Exercises

Each has a solution at the end of `verify/Tutorial.lean`.

1. Prove `theorem applyAll_nil (s : Nat) : applyAll [] s = s`.
2. Prove that the later of two writes to one location wins:
   `set x a (set x b s) = set x a s`. Why does this mean psync must keep two writes to one
   place in order?
3. Prove that a list of functions that each leave the state unchanged leaves it unchanged:
   `(∀ f ∈ fs, ∀ s, f s = s) → applyAll fs s = s`. Which theorem of `Schedule.lean` is this?
4. In `Independent`, why is the condition not symmetric as written? Find the theorem in
   `Schedule.lean` that shows it is symmetric anyway.
5. Suppose psync had a bug that put a read and a write of the same file into one level. Which
   hypothesis of `levels_correct` fails? What does the theorem then say about that program?

<details>
<summary>Answers</summary>

1. `:= rfl`. `applyAll [] s` computes to `s` by the first equation.
2. `funext z; unfold set; by_cases hx : z = x <;> simp_all`. Swapping the writes would leave `b`
   instead of `a`: the two orders give different states, so the writes are not independent
   (output dependence).
3. Induction on `fs`, rewriting the head with the hypothesis (see the file). It is
   `speculation_harmless`, for numbers instead of `Op`s.
4. The first conjunct covers `a`'s writes against `b`'s reads and writes, and the second covers
   `b`'s writes against `a`'s reads. `b`'s writes against `a`'s writes are covered by the first,
   so the condition is symmetric in meaning. `Independent.symm` proves it.
5. Pairwise independence fails for that level, so the `Runs` hypothesis cannot be proved. The
   theorem then says *nothing* about that program. A theorem never covers a case whose hypotheses
   fail. That is why the analyses that establish the hypotheses matter as much as the proof.

</details>
