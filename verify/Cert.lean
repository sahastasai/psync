import World

/-!
# Certified schedules

The theorems of `Schedule.lean` and `World.lean` are about *every* schedule that respects
independence. This file turns them into a check of *one* schedule: the one psync generated for a
given function. psync writes a **certificate** for each rewritten function (its statements'
footprints and effects, and the levels of chains it chose), and `check` decides whether that
schedule is safe. `check_sound` proves that when `check` says yes, every concurrent execution of
the schedule equals the program as written, for any meaning of the statements whose real
independence includes what the certificate claims.

This is translation validation, as in CompCert: rather than proving the macro correct for all
inputs, each build's output carries a certificate that a small, proven checker accepts.

What remains assumed is exactly the hypothesis `hind` of `check_sound`: that the certificate's
footprints and effects describe the statements (checked against MIR by `cargo psync check`, and
the subject of the footprint-soundness work), and, for the outside world, that the commutativity
specification is sound (`World.lean`).
-/

namespace PsyncCert

/-! ## Notation used in the theorem statements

* A certificate $c$ lists the statements' footprints and the levels of chains psync chose;
  $\mathrm{check}(c)$ is the decidable checker.
* $\mathrm{op}_i$ interprets statement $i$ as a step of a system $T$; $[\![ xs ]\!]_T(s)$ runs
  steps in order, and $\mathrm{op}_0, \ldots, \mathrm{op}_{n-1}$ is the program as written.
-/

open PsyncWorld

/-! ## Certificates -/

/-- A client place, as a path of interned segments: `self.groups` might be `[3, 7]`. Two places
overlap when one is a prefix of the other (`self` and `self.groups`).

Some segments are **holes**: the placeholders of a `format!` target, such as `{dir}` in
`format!("{dir}/cache.json")`. A hole may stand for any text, so two places that first differ
where either has a hole overlap; two that first differ at ordinary segments do not. Without holes
this is exactly the prefix relation. -/
abbrev Place := List Nat

def overlap (holes : List Nat) : Place → Place → Bool
  | [], _ => true
  | _, [] => true
  | a :: as, b :: bs => if a == b then overlap holes as bs else holes.contains a || holes.contains b

def anyOverlap (holes : List Nat) (xs ys : List Place) : Bool :=
  xs.any fun p => ys.any fun q => overlap holes p q

/-- One statement as the certificate describes it: the places it may read and write. Every one of
psync's edge rules is a place (`schedule::depends`; the emitter is `cert_nodes` in `schedule.rs`):

- `n/x`: the binding `x`. A statement that introduces it writes it, one that mentions it reads it
  (data flow, rebinding, shadowing).
- `p/a/b`: the access path `a.b` (conflicting accesses; `a` overlaps `a.b`).
- `w/<domain>/<target>`: an effect on the outside world. An unknown target is the domain alone,
  which overlaps every target in it; a process or unknown callee is `w` itself.
- `x/i`: the exit of statement `i`. Statement `i` writes it if it may exit or panic, and a later
  statement with a write effect (not `speculate`) reads it ("may exit before").
- `e`: effect order. Every ordinary effect statement writes it.

Where psync's own rule is finer than prefixes (string-prefix targets, services named by type),
the emitter rounds towards conflict, so the checker may reject a schedule psync accepted, never the
reverse. -/
structure Node where
  reads : List Place
  writes : List Place
  deriving Repr, Inhabited

/-- No edge between two statements: neither writes a place the other reads or writes. -/
def indep (holes : List Nat) (a b : Node) : Bool :=
  !anyOverlap holes a.writes (b.reads ++ b.writes) && !anyOverlap holes b.writes a.reads

/-- A certificate: the statements in program order (node `i` is the `i`-th), and the schedule as
levels of chains of node indices. -/
structure Cert where
  nodes : List Node
  levels : List (List (List Nat))
  /-- The segments that are holes. -/
  holes : List Nat := []
  deriving Repr

def Cert.node (c : Cert) (i : Nat) : Node := c.nodes.getD i default

def Cert.ind (c : Cert) (i j : Nat) : Bool := indep c.holes (c.node i) (c.node j)

/-! ## The checker -/

/-- The first occurrence of `x` in `L`, splitting `L` around it. -/
def split (x : Nat) : List Nat → Option (List Nat × List Nat)
  | [] => none
  | y :: ys => if y = x then some ([], ys) else (split x ys).map fun p => (y :: p.1, p.2)

/-- `L` is `P` with statements moved earlier only past ones independent of them. -/
def reorderable (ind : Nat → Nat → Bool) : List Nat → List Nat → Bool
  | [], L => L.isEmpty
  | x :: P, L =>
    match split x L with
    | none => false
    | some (A, B) => A.all (fun y => ind y x) && reorderable ind P (A ++ B)

/-- The chains of one level are pairwise independent. -/
def pairwiseOK (ind : Nat → Nat → Bool) : List (List Nat) → Bool
  | [] => true
  | ch :: chs => chs.all (fun ch' => ch.all fun x => ch'.all fun y => ind x y) && pairwiseOK ind chs

/-- **The checker**: every level's chains are pairwise independent, and running the levels'
chains one after another is a reordering of the program past independent statements only. -/
def check (c : Cert) : Bool :=
  c.levels.all (pairwiseOK c.ind) &&
    reorderable c.ind (List.range c.nodes.length) (c.levels.map List.flatten).flatten

/-! ## Soundness -/

/-- $$\mathrm{split}(x, L) = (A, B) \implies L = A \mathbin{+\!\!+} [x] \mathbin{+\!\!+} B$$ -/
theorem split_spec {x : Nat} {L A B : List Nat} (h : split x L = some (A, B)) : L = A ++ x :: B := by
  induction L generalizing A B with
  | nil => simp [split] at h
  | cons y ys ih =>
    unfold split at h
    by_cases hy : y = x
    · simp [hy] at h
      obtain ⟨rfl, rfl⟩ := h
      simp [hy]
    · simp only [hy, if_false] at h
      cases hs : split x ys with
      | none => simp [hs] at h
      | some p =>
        simp [hs] at h
        obtain ⟨rfl, rfl⟩ := h
        simp [ih hs]

variable {S A : Type}

/-- $$\bigl(\forall i\,j,\ \mathrm{ind}(i,j) \Rightarrow \mathrm{op}_i \mathrel{I_T} \mathrm{op}_j\bigr) \implies \bigl(\mathrm{reorderable}(P, L) = \mathsf{true} \Rightarrow \mathrm{Reorders}_T(\mathrm{op}(P), \mathrm{op}(L))\bigr)$$ -/
theorem reorderable_sound (T : Sys S A) (op : Nat → A) (ind : Nat → Nat → Bool)
    (hind : ∀ i j, ind i j = true → T.Ind (op i) (op j)) :
    ∀ (P L : List Nat), reorderable ind P L = true → T.Reorders (P.map op) (L.map op) := by
  intro P
  induction P with
  | nil =>
    intro L h
    cases L with
    | nil => exact Sys.Reorders.nil
    | cons _ _ => simp [reorderable] at h
  | cons x P ih =>
    intro L h
    unfold reorderable at h
    cases hs : split x L with
    | none => simp [hs] at h
    | some p =>
      obtain ⟨A', B⟩ := p
      simp only [hs, Bool.and_eq_true, List.all_eq_true] at h
      obtain ⟨hA, hrest⟩ := h
      rw [split_spec hs, List.map_append, List.map_cons, List.map_cons]
      refine Sys.Reorders.cons ?_ ?_
      · intro y hy
        obtain ⟨i, hi, rfl⟩ := List.mem_map.mp hy
        exact hind i x (hA i hi)
      · have := ih (A' ++ B) hrest
        rwa [List.map_append] at this

/-- $$\bigl(\forall i\,j,\ \mathrm{ind}(i,j) \Rightarrow \mathrm{op}_i \mathrel{I_T} \mathrm{op}_j\bigr) \implies \bigl(\mathrm{pairwiseOK}(chs) = \mathsf{true} \Rightarrow \mathrm{PairwiseInd}_T(\mathrm{op}(chs))\bigr)$$ -/
theorem pairwiseOK_sound (T : Sys S A) (op : Nat → A) (ind : Nat → Nat → Bool)
    (hind : ∀ i j, ind i j = true → T.Ind (op i) (op j)) :
    ∀ (chs : List (List Nat)), pairwiseOK ind chs = true →
      T.PairwiseInd (chs.map (List.map op)) := by
  intro chs
  induction chs with
  | nil => intro _; trivial
  | cons ch chs ih =>
    intro h
    simp only [pairwiseOK, Bool.and_eq_true, List.all_eq_true] at h
    obtain ⟨hall, hrest⟩ := h
    refine ⟨?_, ih hrest⟩
    intro c' hc' x hx y hy
    obtain ⟨ch', hch', rfl⟩ := List.mem_map.mp hc'
    obtain ⟨i, hi, rfl⟩ := List.mem_map.mp hx
    obtain ⟨j, hj, rfl⟩ := List.mem_map.mp hy
    exact hind i j (hall ch' hch' i hi j hj)

/-- What can happen at run time: each level's chains interleave in any way (the executor's
choice), and levels run one after another. No independence is assumed here. -/
inductive Execs : List (List (List A)) → List (List A) → Prop
  | nil : Execs [] []
  | cons {cs m levels ms} : InterleaveAll cs m → Execs levels ms → Execs (cs :: levels) (m :: ms)

/-- $$(\forall cs \in \mathit{levels},\ \mathrm{PairwiseInd}_T(cs)) \;\wedge\; \mathrm{Execs}(\mathit{levels}, ms) \implies \mathrm{Runs}_T(\mathit{levels}, ms)$$ -/
theorem execs_runs (T : Sys S A) {levels : List (List (List A))} {ms : List (List A)}
    (hp : ∀ cs ∈ levels, T.PairwiseInd cs) (h : Execs levels ms) : T.Runs levels ms := by
  induction h with
  | nil => exact Sys.Runs.nil
  | @cons cs m levels ms hm _ ih =>
    exact Sys.Runs.cons (hp cs (List.mem_cons_self ..)) hm
      (ih (fun cs' h' => hp cs' (List.mem_cons_of_mem _ h')))

/-- $$\mathrm{flatten}\bigl(\mathrm{map}\ \mathrm{flatten}\ (\mathrm{op}(\mathit{levels}))\bigr) = \mathrm{op}\bigl(\mathrm{flatten}(\mathrm{map}\ \mathrm{flatten}\ \mathit{levels})\bigr)$$ -/
theorem map_flatten_flatten (op : Nat → A) (levels : List (List (List Nat))) :
    ((levels.map (List.map (List.map op))).map List.flatten).flatten =
      ((levels.map List.flatten).flatten).map op := by
  induction levels with
  | nil => rfl
  | cons l ls ih =>
    simp only [List.map_cons, List.flatten_cons, List.map_append, ih, List.map_flatten]

/-- $$\bigl(\forall i\,j,\ c.\mathrm{ind}(i,j) \Rightarrow \mathrm{op}_i \mathrel{I_T} \mathrm{op}_j\bigr) \;\wedge\; \mathrm{check}(c) = \mathsf{true} \;\wedge\; \mathrm{Execs}(\mathrm{op}(c.\mathit{levels}), ms) \implies [\![ \mathrm{flatten}(ms) ]\!]_T(s) = [\![ \mathrm{op}_0, \ldots, \mathrm{op}_{n-1} ]\!]_T(s)$$

**Soundness of the checker.** Interpret each node `i` as a step `op i` of any system `T`
(client memory as in `Schedule.lean`, or the client and server world of `World.lean`). Suppose the
certificate's independence implies real independence (`hind`). Then if `check` accepts the
certificate, every run of its schedule, in any interleaving of each level, ends exactly as the
program run in order does. -/
theorem check_sound (T : Sys S A) (c : Cert) (op : Nat → A)
    (hind : ∀ i j, c.ind i j = true → T.Ind (op i) (op j)) (hc : check c = true)
    {ms : List (List A)} (hx : Execs (c.levels.map (List.map (List.map op))) ms) (s : S) :
    T.runAll ms.flatten s = T.runAll ((List.range c.nodes.length).map op) s := by
  simp only [check, Bool.and_eq_true, List.all_eq_true] at hc
  obtain ⟨hpw, hre⟩ := hc
  have hruns : T.Runs (c.levels.map (List.map (List.map op))) ms := by
    refine execs_runs T ?_ hx
    intro cs hcs
    obtain ⟨l, hl, rfl⟩ := List.mem_map.mp hcs
    exact pairwiseOK_sound T op c.ind hind l (hpw l hl)
  rw [T.levels_correct hruns s, map_flatten_flatten]
  exact T.reorders_correct
    (reorderable_sound T op c.ind hind _ _ hre) s

/-! ## Examples

A certificate the checker accepts: `profile_page` (Basic concepts). `user` (node 0) is read by
`avatar` (node 2); `posts` and `friends` (1, 3) are free. psync's schedule is one level of three
chains: `0 -> 2`, `1`, `3`. Places: `user` is `[10]`, `posts` `[11]`, `avatar` `[12]`,
`friends` `[13]`, `api` `[20]`, and the service behind `api` `[30]`. -/

def profilePage : Cert where
  nodes := [
    { reads := [[20], [30]], writes := [[10]] },
    { reads := [[20], [30]], writes := [[11]] },
    { reads := [[20], [10], [30]], writes := [[12]] },
    { reads := [[20], [30]], writes := [[13]] } ]
  levels := [[[0, 2], [1], [3]]]

/-- $$\mathrm{check}(\mathtt{profilePage}) = \mathsf{true}$$ -/
theorem profilePage_ok : check profilePage = true := by decide

/-- And one it rejects: the same statements with `avatar` scheduled beside `user`, which it reads.
-/
def profilePageWrong : Cert := { profilePage with levels := [[[0], [2], [1], [3]]] }

/-- $$\mathrm{check}(\mathtt{profilePageWrong}) = \mathsf{false}$$ -/
theorem profilePageWrong_rejected : check profilePageWrong = false := by decide

/-- Holes: two writes to `format!("{dir}/a")` and `format!("{dir}/b")` (places `t {dir} / a /` and
`t {dir} / b /`, with `{dir}` the hole `[7]`) may run side by side; a write to
`format!("{other}/a")` may not run beside the first, since `{other}` may equal `{dir}`. -/
def holesOK : Cert where
  nodes := [{ reads := [], writes := [[1, 7, 2, 3, 2]] }, { reads := [], writes := [[1, 7, 2, 4, 2]] }]
  levels := [[[0], [1]]]
  holes := [7, 8]

/-- $$\mathrm{check}(\mathtt{holesOK}) = \mathsf{true}$$ -/
theorem holesOK_ok : check holesOK = true := by decide

def holesClash : Cert :=
  { holesOK with nodes := [{ reads := [], writes := [[1, 7, 2, 3, 2]] }, { reads := [], writes := [[1, 8, 2, 3, 2]] }] }

/-- $$\mathrm{check}(\mathtt{holesClash}) = \mathsf{false}$$ -/
theorem holesClash_rejected : check holesClash = false := by decide

end PsyncCert
