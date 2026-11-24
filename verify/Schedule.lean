/-!
# Why psync's schedules are safe

A small model of what `#[psync::graph]` does, with the three facts its correctness rests on.

* A program is a list of operations on a state (`Loc → Val`). Each operation declares a
  *footprint*: the locations it may read and write. Locations stand for variables and fields
  (the access analysis), and for files, services and other outside resources (the effect
  analysis).
* Two operations are *independent* when neither writes what the other reads or writes. This is
  exactly when psync draws no edge between them.

Proven here:

1. `commute`: independent operations commute.
2. `interleave_correct`: running chains of a level concurrently, under *any* interleaving that keeps
   each chain's own order, gives the same final state as running them one after another, provided
   the chains are pairwise independent. This is what each level's join does.
3. `levels_correct`: a schedule of levels gives the state of running its chains in order.
4. `speculation_harmless`: operations that write nothing (reads) leave the state unchanged, so
   running extra ones after an exit sequential code would have taken changes nothing. psync's
   speculation rule never runs a write early, so only such operations are ever extra.
5. `reorders_correct`: a reordering that moves operations earlier only past independent ones has
   the effect of the program as written.

Not modeled: early exits themselves (psync records them and replays them in program order), the
analyses that compute footprints (the access and effect analyses, and `cargo psync check`), and
the generated Rust (type- and borrow-checked by rustc). The theorems state what those parts must
provide: correct footprints, and edges between every pair of operations that are not
independent.
-/

namespace Psync

variable {Loc Val : Type}

abbrev State (Loc Val : Type) := Loc → Val

/-- An operation with its footprint. `frame`: it changes only what it writes. `local_dep`: what it
writes depends only on what it reads. -/
structure Op (Loc Val : Type) where
  run : State Loc Val → State Loc Val
  R : Loc → Prop
  W : Loc → Prop
  frame : ∀ s l, ¬ W l → run s l = s l
  local_dep : ∀ s s', (∀ l, R l → s l = s' l) → ∀ l, W l → run s l = run s' l

/-- No edge between `a` and `b`: neither writes what the other reads or writes. -/
def Independent (a b : Op Loc Val) : Prop :=
  (∀ l, a.W l → ¬ b.R l ∧ ¬ b.W l) ∧ (∀ l, b.W l → ¬ a.R l)

theorem Independent.symm {a b : Op Loc Val} (h : Independent a b) : Independent b a := by
  refine ⟨fun l hb => ⟨h.2 l hb, fun ha => (h.1 l ha).2 hb⟩, fun l ha => (h.1 l ha).1⟩

/-- **1.** Independent operations commute. -/
theorem commute (a b : Op Loc Val) (h : Independent a b) (s : State Loc Val) :
    b.run (a.run s) = a.run (b.run s) := by
  funext l
  by_cases ha : a.W l
  · have hb : ¬ b.W l := (h.1 l ha).2
    rw [b.frame _ _ hb]
    -- `b` writes nothing `a` reads, so `a` sees the same inputs before and after `b`.
    exact a.local_dep s (b.run s)
      (fun l' hr => (b.frame s l' (fun hw => h.2 l' hw hr)).symm) l ha
  · by_cases hb : b.W l
    · rw [a.frame _ _ ha]
      exact b.local_dep (a.run s) s
        (fun l' hr => a.frame s l' (fun hw => (h.1 l' hw).1 hr)) l hb
    · rw [b.frame _ _ hb, a.frame _ _ ha, a.frame _ _ ha, b.frame _ _ hb]

/-- Running a list of operations in order. -/
def runAll : List (Op Loc Val) → State Loc Val → State Loc Val
  | [], s => s
  | o :: os, s => runAll os (o.run s)

theorem runAll_append (xs ys : List (Op Loc Val)) (s : State Loc Val) :
    runAll (xs ++ ys) s = runAll ys (runAll xs s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih => exact ih _

/-- An operation independent of every operation of a list commutes with the whole list. -/
theorem commute_list (y : Op Loc Val) (xs : List (Op Loc Val))
    (h : ∀ x ∈ xs, Independent x y) (s : State Loc Val) :
    runAll xs (y.run s) = y.run (runAll xs s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih =>
    show runAll xs (x.run (y.run s)) = y.run (runAll xs (x.run s))
    rw [← commute x y (h x (List.mem_cons_self ..)) s]
    exact ih (fun x' hx => h x' (List.mem_cons_of_mem _ hx)) _

/-- `m` interleaves the chains `a` and `b`, keeping the order within each. -/
inductive Interleave : List (Op Loc Val) → List (Op Loc Val) → List (Op Loc Val) → Prop
  | nil : Interleave [] [] []
  | left {x a b m} : Interleave a b m → Interleave (x :: a) b (x :: m)
  | right {y a b m} : Interleave a b m → Interleave a (y :: b) (y :: m)

/-- **2.** Concurrent chains: any interleaving of two independent chains has the effect of running
the first, then the second. (For more chains, interleave one with the interleaving of the rest.) -/
theorem interleave_correct {a b m : List (Op Loc Val)}
    (hab : ∀ x ∈ a, ∀ y ∈ b, Independent x y) (hm : Interleave a b m) (s : State Loc Val) :
    runAll m s = runAll (a ++ b) s := by
  induction hm generalizing s with
  | nil => rfl
  | @left x a b m _ ih =>
    exact ih (fun x' hx y hy => hab x' (List.mem_cons_of_mem _ hx) y hy) _
  | @right y a b m _ ih =>
    show runAll m (y.run s) = runAll (a ++ y :: b) s
    rw [ih (fun x hx y' hy => hab x hx y' (List.mem_cons_of_mem _ hy)), runAll_append,
      runAll_append, commute_list y a (fun x hx => hab x hx y (List.mem_cons_self ..))]
    rfl

/-- Interleaving several chains: the first with an interleaving of the rest. -/
inductive InterleaveAll : List (List (Op Loc Val)) → List (Op Loc Val) → Prop
  | nil : InterleaveAll [] []
  | cons {c cs m r} : InterleaveAll cs r → Interleave c r m → InterleaveAll (c :: cs) m

theorem interleaveAll_mem {cs : List (List (Op Loc Val))} {m : List (Op Loc Val)}
    (h : InterleaveAll cs m) : ∀ y ∈ m, ∃ c ∈ cs, y ∈ c := by
  induction h with
  | nil => intro y hy; cases hy
  | @cons c cs m r _ hi ih =>
    intro y hy
    -- every element of an interleaving comes from one of its two sides
    have side : ∀ {a b m : List (Op Loc Val)}, Interleave a b m → ∀ z ∈ m, z ∈ a ∨ z ∈ b := by
      intro a b m hi
      induction hi with
      | nil => intro z hz; cases hz
      | @left x a b m _ ih' =>
        intro z hz
        cases hz with
        | head => exact Or.inl (List.mem_cons_self ..)
        | tail _ hz => exact (ih' z hz).imp (List.mem_cons_of_mem _) id
      | @right y a b m _ ih' =>
        intro z hz
        cases hz with
        | head => exact Or.inr (List.mem_cons_self ..)
        | tail _ hz => exact (ih' z hz).imp id (List.mem_cons_of_mem _)
    rcases side hi y hy with h | h
    · exact ⟨c, List.mem_cons_self .., h⟩
    · obtain ⟨c', hc', hy'⟩ := ih y h
      exact ⟨c', List.mem_cons_of_mem _ hc', hy'⟩

/-- Chains of one level are pairwise independent: no edge between any two of their operations. -/
def PairwiseIndependent : List (List (Op Loc Val)) → Prop
  | [] => True
  | c :: cs => (∀ c' ∈ cs, ∀ x ∈ c, ∀ y ∈ c', Independent x y) ∧ PairwiseIndependent cs

/-- A whole level: any interleaving of pairwise independent chains has the effect of running the
chains one after another. -/
theorem level_correct {cs : List (List (Op Loc Val))} {m : List (Op Loc Val)}
    (hp : PairwiseIndependent cs) (hm : InterleaveAll cs m) (s : State Loc Val) :
    runAll m s = runAll cs.flatten s := by
  induction hm generalizing s with
  | nil => rfl
  | @cons c cs m r hr hi ih =>
    have indep : ∀ x ∈ c, ∀ y ∈ r, Independent x y := by
      intro x hx y hy
      obtain ⟨c', hc', hy'⟩ := interleaveAll_mem hr y hy
      exact hp.1 c' hc' x hx y hy'
    rw [interleave_correct indep hi, runAll_append, List.flatten_cons, runAll_append,
      ih hp.2]

/-- How a schedule runs: each level's chains, pairwise independent, in some interleaving. -/
inductive Runs : List (List (List (Op Loc Val))) → List (List (Op Loc Val)) → Prop
  | nil : Runs [] []
  | cons {cs m levels ms} :
      PairwiseIndependent cs → InterleaveAll cs m → Runs levels ms → Runs (cs :: levels) (m :: ms)

/-- **3.** A schedule is a list of levels run one after another: whatever interleaving each level
takes, the result is that of running every chain in order. -/
theorem levels_correct {levels : List (List (List (Op Loc Val)))} {ms : List (List (Op Loc Val))}
    (h : Runs levels ms) (s : State Loc Val) :
    runAll ms.flatten s = runAll (levels.map List.flatten).flatten s := by
  induction h generalizing s with
  | nil => rfl
  | @cons cs m levels ms hp hm _ ih =>
    rw [List.flatten_cons, runAll_append, List.map_cons, List.flatten_cons, runAll_append,
      level_correct hp hm, ih]

/-- **4.** Operations that write nothing leave the state as it was. -/
theorem speculation_harmless (extra : List (Op Loc Val)) (hread : ∀ o ∈ extra, ∀ l, ¬ o.W l)
    (s : State Loc Val) : runAll extra s = s := by
  induction extra generalizing s with
  | nil => rfl
  | cons o os ih =>
    show runAll os (o.run s) = s
    have : o.run s = s := funext fun l => o.frame s l (hread o (List.mem_cons_self ..) l)
    rw [this]
    exact ih (fun o' ho => hread o' (List.mem_cons_of_mem _ ho)) s

/-- `Reorders P L`: `L` is `P` with operations moved earlier only past operations independent of
them. Concretely, the first operation `x` of `P` sits in `L` after a run `A` of operations that are
all independent of `x`, and the rest of `L` reorders the rest of `P` the same way. psync's
schedule (levels of chains, from the dependency graph) has this property: an operation scheduled
ahead of an earlier one has no edge to it, and psync draws an edge exactly when two operations are
not independent. -/
inductive Reorders : List (Op Loc Val) → List (Op Loc Val) → Prop
  | nil : Reorders [] []
  | cons {x P A B} : (∀ y ∈ A, Independent y x) → Reorders P (A ++ B) →
      Reorders (x :: P) (A ++ x :: B)

/-- **5.** Such a reordering has the effect of the program as written. Together with
`levels_correct`, the concurrent execution of psync's schedule ends in the state the sequential
program ends in. -/
theorem reorders_correct {P L : List (Op Loc Val)} (h : Reorders P L) (s : State Loc Val) :
    runAll L s = runAll P s := by
  induction h generalizing s with
  | nil => rfl
  | @cons x P A B hA _ ih =>
    show runAll (A ++ x :: B) s = runAll P (x.run s)
    rw [runAll_append]
    show runAll B (x.run (runAll A s)) = runAll P (x.run s)
    rw [← commute_list x A hA, ← runAll_append, ih]

end Psync
