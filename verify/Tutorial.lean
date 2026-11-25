/-!
# Lean from zero, for reading `Schedule.lean`

The book's chapter "Proofs" (Foundations) walks through this file. It uses only Lean's core
library, like `Schedule.lean`, and every example is a smaller version of something there.

    lean verify/Tutorial.lean      # prints nothing when every proof checks
-/

namespace Tutorial

/-! ## 1. Definitions, and proofs by computation -/

def double (n : Nat) : Nat := n + n

/-- `rfl`: both sides compute to the same value. -/
theorem double_21 : double 21 = 42 := rfl

/-- For *every* `n`, which no amount of computing can check: unfold, then arithmetic. -/
theorem double_eq (n : Nat) : double n = 2 * n := by
  unfold double
  omega

/-! ## 2. Programs as lists of steps (`runAll` in `Schedule.lean`) -/

/-- Run a list of state transformers in order. -/
def applyAll : List (Nat → Nat) → Nat → Nat
  | [], s => s
  | f :: fs, s => applyAll fs (f s)

/-- Running `xs ++ ys` is running `xs`, then `ys`. Proof by induction on `xs`. -/
theorem applyAll_append (xs ys : List (Nat → Nat)) (s : Nat) :
    applyAll (xs ++ ys) s = applyAll ys (applyAll xs s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih => exact ih (x s)

/-- Two steps that commute can be swapped (`commute` in `Schedule.lean`, for two steps). -/
theorem swap2 (f g : Nat → Nat) (h : ∀ s, f (g s) = g (f s)) (s : Nat) :
    applyAll [f, g] s = applyAll [g, f] s := by
  show g (f s) = f (g s)
  rw [h]

/-! ## 3. State as a function from locations to values (`State` in `Schedule.lean`) -/

abbrev State := String → Nat

/-- Write `v` to location `x`. -/
def set (x : String) (v : Nat) (s : State) : State := fun y => if y = x then v else s y

/-- Writes to different locations commute: the simplest case of `commute`. -/
theorem set_comm (x y : String) (a b : Nat) (hxy : x ≠ y) (s : State) :
    set x a (set y b s) = set y b (set x a s) := by
  funext z
  unfold set
  by_cases hx : z = x <;> by_cases hy : z = y <;> simp_all

/-! ## 4. The rest of what `Schedule.lean` uses -/

/-- A set of locations as a predicate: `Small l` is the proposition "`l` is in the set".
`Schedule.lean`'s footprints `R` and `W` are sets like this, `Loc → Prop`. -/
def Small : Nat → Prop := fun n => n < 10

/-- A `structure` bundles data with proofs about it, as `Op` bundles `run` with `frame`. -/
structure Bump where
  step : Nat → Nat
  grows : ∀ n, n ≤ step n

def addOne : Bump := ⟨fun n => n + 1, fun n => Nat.le_succ n⟩

/-- Implicit arguments `{..}` are filled in by Lean; `h.1` and `h.2` take apart an `∧`. -/
theorem both_small {a b : Nat} (h : Small a ∧ Small b) : a + b < 20 := by
  have h1 := h.1
  have h2 := h.2
  unfold Small at h1 h2
  omega

/-- An `inductive` predicate is defined by rules, like `Interleave` and `Runs`: a number is
`Even` by `zero`, or by `step` from an even number two below. -/
inductive Even : Nat → Prop
  | zero : Even 0
  | step {n : Nat} : Even n → Even (n + 2)

theorem four_even : Even 4 := Even.step (Even.step Even.zero)

/-- Proofs about an inductive predicate go by induction over its rules. -/
theorem even_double {n : Nat} (h : Even n) : ∃ k, n = 2 * k := by
  induction h with
  | zero => exact ⟨0, rfl⟩
  | step _ ih =>
    obtain ⟨k, hk⟩ := ih
    exact ⟨k + 1, by omega⟩

/-! ## 5. Solutions to the chapter's exercises -/

/-- Exercise 1. -/
theorem applyAll_nil (s : Nat) : applyAll [] s = s := rfl

/-- Exercise 2: the later write wins (why psync keeps two writes to one place in order). -/
theorem set_set (x : String) (a b : Nat) (s : State) : set x a (set x b s) = set x a s := by
  funext z
  unfold set
  by_cases hx : z = x <;> simp_all

/-- Exercise 3: steps that change nothing can be run any number of times
(`speculation_harmless` in `Schedule.lean`). -/
theorem identities_harmless (fs : List (Nat → Nat)) (h : ∀ f ∈ fs, ∀ s, f s = s) (s : Nat) :
    applyAll fs s = s := by
  induction fs generalizing s with
  | nil => rfl
  | cons f fs ih =>
    show applyAll fs (f s) = s
    rw [h f (List.mem_cons_self ..) s]
    exact ih (fun g hg => h g (List.mem_cons_of_mem _ hg)) s

end Tutorial
