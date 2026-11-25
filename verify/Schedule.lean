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

6. `replay_correct`: recording every statement's exit outcome and replaying the first one in
   program order gives the state and the exit of sequential execution.
7. `recording_slot`: recording an outcome in a slot is an ordinary operation.

Not modeled: the
analyses that compute footprints (the access and effect analyses, and `cargo psync check`), and
the generated Rust (type- and borrow-checked by rustc). The theorems state what those parts must
provide: correct footprints, and edges between every pair of operations that are not
independent.
-/

namespace Psync

/-! ## Notation used in the theorem statements

* $s : \mathrm{Loc} \to \mathrm{Val}$ is a state; an operation $o$ has a step $\mathrm{run}_o$, a read
  footprint $R_o$ and a write footprint $W_o$.
* $[\![ xs ]\!](s)$ runs the list of operations $xs$ in order from $s$ (`runAll`);
  $\mathbin{+\!\!+}$ is list concatenation.
* $a \mathrel{I} b$ (`Independent`) means $W_a \cap (R_b \cup W_b) = \emptyset$ and
  $W_b \cap R_a = \emptyset$.
* $a \parallel b$ is the set of interleavings of $a$ and $b$ that keep each one's order
  (`Interleave`); $\Vert_{c \in cs}\, c$ interleaves several chains (`InterleaveAll`).
-/

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

/-- $$a \mathrel{I} b \implies b \mathrel{I} a$$ -/
theorem Independent.symm {a b : Op Loc Val} (h : Independent a b) : Independent b a := by
  refine ⟨fun l hb => ⟨h.2 l hb, fun ha => (h.1 l ha).2 hb⟩, fun l ha => (h.1 l ha).1⟩

/-- $$a \mathrel{I} b \implies \mathrm{run}_b(\mathrm{run}_a(s)) = \mathrm{run}_a(\mathrm{run}_b(s))$$

**1.** Independent operations commute. -/
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

/-- $$[\![ xs \mathbin{+\!\!+} ys ]\!](s) = [\![ ys ]\!]([\![ xs ]\!](s))$$ -/
theorem runAll_append (xs ys : List (Op Loc Val)) (s : State Loc Val) :
    runAll (xs ++ ys) s = runAll ys (runAll xs s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih => exact ih _

/-- $$(\forall x \in xs,\ x \mathrel{I} y) \implies [\![ xs ]\!](\mathrm{run}_y(s)) = \mathrm{run}_y\bigl([\![ xs ]\!](s)\bigr)$$

An operation independent of every operation of a list commutes with the whole list. -/
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

/-- $$(\forall x \in a,\ \forall y \in b,\ x \mathrel{I} y) \;\wedge\; m \in a \parallel b \implies [\![ m ]\!](s) = [\![ a \mathbin{+\!\!+} b ]\!](s)$$

**2.** Concurrent chains: any interleaving of two independent chains has the effect of running
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

/-- $$m \in \textstyle\Vert_{c \in cs}\, c \implies \forall y \in m,\ \exists c \in cs,\ y \in c$$ -/
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

/-- $$\mathrm{PairwiseInd}(cs) \;\wedge\; m \in \textstyle\Vert_{c \in cs}\, c \implies [\![ m ]\!](s) = [\![ c_1 \mathbin{+\!\!+} \cdots \mathbin{+\!\!+} c_k ]\!](s)$$

A whole level: any interleaving of pairwise independent chains has the effect of running the
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

/-- $$\mathrm{Runs}(\mathit{levels}, ms) \implies [\![ \mathrm{flatten}(ms) ]\!](s) = [\![ \mathrm{flatten}(\mathrm{map}\ \mathrm{flatten}\ \mathit{levels}) ]\!](s)$$

**3.** A schedule is a list of levels run one after another: whatever interleaving each level
takes, the result is that of running every chain in order. -/
theorem levels_correct {levels : List (List (List (Op Loc Val)))} {ms : List (List (Op Loc Val))}
    (h : Runs levels ms) (s : State Loc Val) :
    runAll ms.flatten s = runAll (levels.map List.flatten).flatten s := by
  induction h generalizing s with
  | nil => rfl
  | @cons cs m levels ms hp hm _ ih =>
    rw [List.flatten_cons, runAll_append, List.map_cons, List.flatten_cons, runAll_append,
      level_correct hp hm, ih]

/-- $$(\forall o \in \mathit{extra},\ W_o = \emptyset) \implies [\![ \mathit{extra} ]\!](s) = s$$

**4.** Operations that write nothing leave the state as it was. -/
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

/-- $$\mathrm{Reorders}(P, L) \implies [\![ L ]\!](s) = [\![ P ]\!](s)$$

**5.** Such a reordering has the effect of the program as written. Together with
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

/-! ## Early exits

A statement such as `let x = f().await?` may *exit*: whether it does, and with which value, depends
on what it reads. psync never takes an exit inside a chain: it records the outcome in an exit slot
and stops only that chain, and after the join it replays the recorded exits in program order
(`exits.rs`). Writes never start before an earlier statement that may exit, so the only work done
after the first exit is reads.

The theorems below say that this gives exactly the sequential behaviour: the same final state and
the same exit. -/

/-- An operation that may exit, with an outcome that depends only on what it reads. -/
structure XOp (Loc Val E : Type) where
  op : Op Loc Val
  exit : State Loc Val → Option E
  exit_dep : ∀ s s', (∀ l, op.R l → s l = s' l) → exit s = exit s'

variable {E : Type}

/-- Sequential execution with exits: run each operation; stop at the first that exits. -/
def seqX : List (XOp Loc Val E) → State Loc Val → State Loc Val × Option E
  | [], s => (s, none)
  | x :: xs, s =>
    match x.exit s with
    | some e => (x.op.run s, some e)
    | none => seqX xs (x.op.run s)

/-- Every operation's outcome, in program order, when all of them run (what the exit slots hold). -/
def outcomes : List (XOp Loc Val E) → State Loc Val → List (Option E)
  | [], _ => []
  | x :: xs, s => x.exit s :: outcomes xs (x.op.run s)

/-- Replay: the first recorded exit in program order. -/
def firstExit : List (Option E) → Option E
  | [] => none
  | some e :: _ => some e
  | none :: r => firstExit r

/-- psync's speculation rule, along a run: every operation after the first one that exits writes
nothing. -/
def SpecSafe : List (XOp Loc Val E) → State Loc Val → Prop
  | [], _ => True
  | x :: xs, s =>
    match x.exit s with
    | some _ => ∀ y ∈ xs, ∀ l, ¬ y.op.W l
    | none => SpecSafe xs (x.op.run s)

/-- $$\mathrm{SpecSafe}(xs, s) \implies \bigl([\![ \mathrm{op}(xs) ]\!](s),\ \mathrm{firstExit}(\mathrm{outcomes}(xs, s))\bigr) = \mathrm{seq}_X(xs, s)$$

**6.** Running every operation, recording the outcomes and replaying the first exit in program
order gives the state and the exit of sequential execution, provided only reads run past the
first exit. -/
theorem replay_correct (xs : List (XOp Loc Val E)) (s : State Loc Val) (h : SpecSafe xs s) :
    (runAll (xs.map XOp.op) s, firstExit (outcomes xs s)) = seqX xs s := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih =>
    simp only [SpecSafe] at h
    simp only [List.map_cons, runAll, outcomes, seqX]
    cases hx : x.exit s with
    | some e =>
      rw [hx] at h
      simp only [firstExit]
      rw [speculation_harmless (xs.map XOp.op) (by
        intro o ho l
        obtain ⟨y, hy, rfl⟩ := List.mem_map.mp ho
        exact h y hy l)]
    | none =>
      rw [hx] at h
      simp only [firstExit]
      exact ih _ h

/-- What psync's generated code runs for a statement that may exit: the statement, which also
writes its outcome into its own exit slot `k` (encoded as a value by `enc`). -/
def XOp.recording [DecidableEq Loc] (x : XOp Loc Val E) (k : Loc) (enc : Option E → Val) :
    Op Loc Val where
  run s := fun l => if l = k then enc (x.exit s) else x.op.run s l
  R := x.op.R
  W := fun l => x.op.W l ∨ l = k
  frame := by
    intro s l h
    have hk : ¬ l = k := fun e => h (Or.inr e)
    simp only [if_neg hk]
    exact x.op.frame s l (fun w => h (Or.inl w))
  local_dep := by
    intro s s' hR l hW
    by_cases hk : l = k
    · simp only [if_pos hk]
      rw [x.exit_dep s s' hR]
    · simp only [if_neg hk]
      exact x.op.local_dep s s' hR l (hW.resolve_right hk)

/-- $$\mathrm{run}_{\mathrm{rec}_k(x)}(s)(k) = \mathrm{enc}\bigl(\mathrm{exit}_x(s)\bigr)$$

**7.** The slot holds the statement's outcome. Since `recording` is an ordinary `Op`, every
theorem above applies to programs whose statements record their exits: a schedule of independent
chains (independence now includes the slots, which no other statement touches) leaves the same
values in every slot as program order, so the replay reads the same outcomes. -/
theorem recording_slot [DecidableEq Loc] (x : XOp Loc Val E) (k : Loc) (enc : Option E → Val)
    (s : State Loc Val) : (x.recording k enc).run s k = enc (x.exit s) := by
  simp [XOp.recording]

end Psync
