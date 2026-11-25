import Cert

/-!
# Footprint soundness

`check_sound` (`Cert.lean`) proves a certified schedule correct *given* `hind`: that the
certificate's places describe what the statements really touch. This file proves `hind` for a
fragment of MIR, so that for programs in the fragment nothing about footprints stays assumed.

1. **The fragment** (`Stmt`): places (a local and a path of field projections), assignments from
   operands, moves of whole aggregates, sequencing, `if`, `while`, and calls that pass places by
   reference. Execution has fuel (`exec`), so loops and recursion are total; every theorem holds
   for every fuel, so for every terminating run.
2. **Summaries** (`fp`): the places a statement may read and write, computed from the statement's
   syntax and, for a call, from the callee's **summary** with parameters replaced by the
   arguments. This is psync's cross-crate scheme: a crate's summaries are computed from its MIR
   and stored per function; a caller uses them without seeing the callee's body.
3. **The link check** (`Valid`): every summary covers the footprint of its function's body,
   computed with the summaries. It is a local check per function, so it handles recursion and
   functions from other crates alike; it is decidable for a finite program (`Program.validB`).
4. **Theorems**: if the summaries are valid, a statement changes nothing outside its writes
   (`frame`), and what it writes depends only on what it reads and writes (`dep`). So statements
   with disjoint footprints commute (`memSys`), certificate independence implies real
   independence (`hind_of_cover`), and `mir_schedule_sound` is `check_sound` with no `hind` left:
   for a valid program and a certificate that covers the footprints, every run of the schedule
   ends as the sequential program does.

What the fragment leaves out, and so stays assumed for real Rust: references stored in memory and
dereferenced (`*p`: the borrow checker's aliasing guarantees are what lets psync name places, and
they are not modelled), interior mutability and `unsafe`, panics and unwinding, and the
correspondence between real MIR and this fragment.

    lake build PsyncVerify
-/

namespace PsyncFootprint

/-! ## Notation used in the theorem statements

* A place $p = (p_1, p_2)$ is a local and a path of projections; $p \sqsubseteq q$ (`pre`): $p$ is a
  prefix of $q$, so writing $p$ may change $q$.
* ${\downarrow}xs$ (`under`): the places at or below some place of $xs$;
  $\mathrm{covers}(xs, ys)$: ${\downarrow}ys \subseteq {\downarrow}xs$.
* $p[\sigma]$ substitutes the arguments $\sigma$ for a callee's parameters.
* $\mathrm{fp}_S(s) = (R_S(s), W_S(s))$ is statement $s$'s footprint computed with the summaries
  $S$, and $T_S(s) = R_S(s) \mathbin{+\!\!+} W_S(s)$ is everything it touches.
* $\mathrm{exec}^n_P(s, m)$ runs $s$ from memory $m$ with fuel $n$ in program $P$;
  $\mathrm{Valid}(S, P)$ is the link check: every summary covers its body's footprint.
-/

open PsyncWorld PsyncCert

/-! ## Places and prefixes -/

/-- A place: a local and a path of projections (`_3.0.1` is `(3, [0, 1])`). -/
abbrev Pl := Nat × List Nat

/-- `lpre a b`: the list `a` is a prefix of `b`. -/
def lpre : List Nat → List Nat → Bool
  | [], _ => true
  | _ :: _, [] => false
  | a :: as, b :: bs => a == b && lpre as bs

/-- $$\mathrm{lpre}(a, b) = \mathsf{true} \iff \exists t,\ b = a \mathbin{+\!\!+} t$$ -/
theorem lpre_iff : ∀ (a b : List Nat), lpre a b = true ↔ ∃ t, b = a ++ t
  | [], b => by simp [lpre]
  | a :: as, [] => by simp [lpre]
  | a :: as, b :: bs => by
    simp only [lpre, Bool.and_eq_true, beq_iff_eq, List.cons_append, List.cons.injEq]
    rw [lpre_iff as bs]
    constructor
    · rintro ⟨rfl, t, rfl⟩; exact ⟨t, rfl, rfl⟩
    · rintro ⟨t, rfl, rfl⟩; exact ⟨rfl, t, rfl⟩

/-- `pre p q`: place `p` contains place `q` (`_3` contains `_3.0.1`). -/
def pre (p q : Pl) : Bool := p.1 == q.1 && lpre p.2 q.2

/-- $$p \sqsubseteq q \iff p_1 = q_1 \wedge \exists t,\ q_2 = p_2 \mathbin{+\!\!+} t$$ -/
theorem pre_iff (p q : Pl) : pre p q = true ↔ p.1 = q.1 ∧ ∃ t, q.2 = p.2 ++ t := by
  simp [pre, lpre_iff]

/-- $$p \sqsubseteq p$$ -/
theorem pre_refl (p : Pl) : pre p p = true := (pre_iff p p).2 ⟨rfl, [], by simp⟩

/-- $$p \sqsubseteq q \wedge q \sqsubseteq r \implies p \sqsubseteq r$$ -/
theorem pre_trans {p q r : Pl} (h1 : pre p q = true) (h2 : pre q r = true) : pre p r = true := by
  obtain ⟨e1, t1, h1⟩ := (pre_iff _ _).1 h1
  obtain ⟨e2, t2, h2⟩ := (pre_iff _ _).1 h2
  exact (pre_iff _ _).2 ⟨e1.trans e2, t1 ++ t2, by rw [h2, h1, List.append_assoc]⟩

/-- Some place of `ps` contains `l`. -/
def under (ps : List Pl) (l : Pl) : Prop := ∃ p ∈ ps, pre p l = true

/-- Every place of `ys` is contained in a place of `xs`. -/
def covers (xs ys : List Pl) : Prop := ∀ y ∈ ys, ∃ x ∈ xs, pre x y = true

/-- $$\mathrm{covers}(xs, ys) \wedge l \in {\downarrow}ys \implies l \in {\downarrow}xs$$ -/
theorem under_of_covers {xs ys : List Pl} (h : covers xs ys) {l : Pl} (hl : under ys l) :
    under xs l := by
  obtain ⟨y, hy, hyl⟩ := hl
  obtain ⟨x, hx, hxy⟩ := h y hy
  exact ⟨x, hx, pre_trans hxy hyl⟩

/-- $$l \in {\downarrow}(xs \mathbin{+\!\!+} ys) \iff l \in {\downarrow}xs \vee l \in {\downarrow}ys$$ -/
theorem under_append {xs ys : List Pl} {l : Pl} : under (xs ++ ys) l ↔ under xs l ∨ under ys l := by
  simp only [under, List.mem_append]
  constructor
  · rintro ⟨p, hp | hp, h⟩
    · exact Or.inl ⟨p, hp, h⟩
    · exact Or.inr ⟨p, hp, h⟩
  · rintro (⟨p, hp, h⟩ | ⟨p, hp, h⟩)
    · exact ⟨p, Or.inl hp, h⟩
    · exact ⟨p, Or.inr hp, h⟩

/-- $$ys \subseteq xs \wedge l \in {\downarrow}ys \implies l \in {\downarrow}xs$$ -/
theorem under_mono {xs ys : List Pl} (h : ∀ p ∈ ys, p ∈ xs) {l : Pl} (hl : under ys l) :
    under xs l := by
  obtain ⟨p, hp, hpl⟩ := hl
  exact ⟨p, h p hp, hpl⟩

/-- $$l \in {\downarrow}[p] \iff p \sqsubseteq l$$ -/
theorem under_single {p l : Pl} : under [p] l ↔ pre p l = true := by simp [under]

/-! ## Substitution: a callee's places in the caller's terms -/

/-- `σ k` is the caller's place passed for the callee's local `k` (its parameters, and fresh
caller places for its temporaries). The callee's `_k.f` becomes `σ k` followed by `.f`. -/
def subst (σ : Nat → Pl) (p : Pl) : Pl := ((σ p.1).1, (σ p.1).2 ++ p.2)

/-- $$p[\tau][\sigma] = p\bigl[k \mapsto \tau(k)[\sigma]\bigr]$$ -/
theorem subst_comp (σ τ : Nat → Pl) (p : Pl) :
    subst σ (subst τ p) = subst (fun k => subst σ (τ k)) p := by
  simp [subst, List.append_assoc]

/-- $$p \sqsubseteq q \implies p[\sigma] \sqsubseteq q[\sigma]$$ -/
theorem pre_subst (σ : Nat → Pl) {p q : Pl} (h : pre p q = true) :
    pre (subst σ p) (subst σ q) = true := by
  obtain ⟨e, t, ht⟩ := (pre_iff _ _).1 h
  refine (pre_iff _ _).2 ⟨?_, t, ?_⟩
  · simp [subst, e]
  · simp [subst, e, ht, List.append_assoc]

/-- $$\mathrm{covers}(xs, ys) \implies \mathrm{covers}(xs[\sigma], ys[\sigma])$$ -/
theorem covers_map (σ : Nat → Pl) {xs ys : List Pl} (h : covers xs ys) :
    covers (xs.map (subst σ)) (ys.map (subst σ)) := by
  intro y hy
  obtain ⟨y', hy', rfl⟩ := List.mem_map.1 hy
  obtain ⟨x, hx, hxy⟩ := h y' hy'
  exact ⟨subst σ x, List.mem_map.2 ⟨x, hx, rfl⟩, pre_subst σ hxy⟩

/-! ## The fragment -/

/-- An operand: a copy of a (scalar) place, or a constant. -/
inductive Op
  | place (p : Pl)
  | const (v : Int)

abbrev Mem := Pl → Int

def Op.eval : Op → Mem → Int
  | .place p, m => m p
  | .const v, _ => v

def Op.reads : Op → List Pl
  | .place p => [p]
  | .const _ => []

def Op.subst (σ : Nat → Pl) : Op → Op
  | .place p => .place (PsyncFootprint.subst σ p)
  | .const v => .const v

/-- A MIR-like statement. `assign d f a b` is `d = f(a, b)` for any pure operation `f` (a binary
operator, a cast, a plain copy). `move d s` copies the whole aggregate at `s` to `d`. `call f σ`
runs function `f`'s body with its locals mapped to the caller's places by `σ`. -/
inductive Stmt
  | nop
  | assign (d : Pl) (f : Int → Int → Int) (a b : Op)
  | move (d s : Pl)
  | seq (a b : Stmt)
  | ite (c : Pl) (t e : Stmt)
  | while (c : Pl) (body : Stmt)
  | call (f : Nat) (σ : Nat → Pl)

/-- A statement of a callee, in the caller's terms. -/
def Stmt.subst (σ : Nat → Pl) : Stmt → Stmt
  | .nop => .nop
  | .assign d f a b => .assign (PsyncFootprint.subst σ d) f (a.subst σ) (b.subst σ)
  | .move d s => .move (PsyncFootprint.subst σ d) (PsyncFootprint.subst σ s)
  | .seq a b => .seq (a.subst σ) (b.subst σ)
  | .ite c t e => .ite (PsyncFootprint.subst σ c) (t.subst σ) (e.subst σ)
  | .while c b => .while (PsyncFootprint.subst σ c) (b.subst σ)
  | .call g τ => .call g (fun k => PsyncFootprint.subst σ (τ k))

/-- Execution with fuel. `P f` is the body of function `f`. Out of fuel, a statement stops where it
is; the theorems hold for every fuel, so in particular for every run that terminates. -/
def exec (P : Nat → Stmt) : Nat → Stmt → Mem → Mem
  | 0, _, m => m
  | _ + 1, .nop, m => m
  | _ + 1, .assign d f a b, m => fun l => if l = d then f (a.eval m) (b.eval m) else m l
  | _ + 1, .move d s, m => fun l =>
      if pre d l then m (s.1, s.2 ++ l.2.drop d.2.length) else m l
  | n + 1, .seq a b, m => exec P n b (exec P n a m)
  | n + 1, .ite c t e, m => if m c ≠ 0 then exec P n t m else exec P n e m
  | n + 1, .while c b, m => if m c ≠ 0 then exec P n (.while c b) (exec P n b m) else m
  | n + 1, .call f σ, m => exec P n ((P f).subst σ) m

/-! ## Summaries -/

/-- A footprint: the places a statement may read, and those it may write. -/
abbrev Fp := List Pl × List Pl

/-- The footprint of a statement, given a summary `S f` for every function, in the callee's terms.
This is what psync computes from MIR, per function, and stores per crate. -/
def fp (S : Nat → Fp) : Stmt → Fp
  | .nop => ([], [])
  | .assign d _ a b => (a.reads ++ b.reads, [d])
  | .move d s => ([s], [d])
  | .seq a b => ((fp S a).1 ++ (fp S b).1, (fp S a).2 ++ (fp S b).2)
  | .ite c t e => (c :: ((fp S t).1 ++ (fp S e).1), (fp S t).2 ++ (fp S e).2)
  | .while c b => (c :: (fp S b).1, (fp S b).2)
  | .call f σ => ((S f).1.map (subst σ), (S f).2.map (subst σ))

/-- $$R(o[\sigma]) = R(o)[\sigma]$$ -/
theorem Op.reads_subst (σ : Nat → Pl) (o : Op) :
    (o.subst σ).reads = o.reads.map (PsyncFootprint.subst σ) := by
  cases o <;> rfl

/-- $$\mathrm{fp}_S(s[\sigma]) = \bigl(R_S(s)[\sigma],\ W_S(s)[\sigma]\bigr)$$

The footprint of a substituted statement is the substituted footprint. -/
theorem fp_subst (S : Nat → Fp) (σ : Nat → Pl) :
    ∀ s : Stmt, fp S (s.subst σ) = ((fp S s).1.map (subst σ), (fp S s).2.map (subst σ))
  | .nop => rfl
  | .assign d f a b => by simp [Stmt.subst, fp, Op.reads_subst]
  | .move d s => rfl
  | .seq a b => by simp [Stmt.subst, fp, fp_subst S σ a, fp_subst S σ b]
  | .ite c t e => by simp [Stmt.subst, fp, fp_subst S σ t, fp_subst S σ e]
  | .while c b => by simp [Stmt.subst, fp, fp_subst S σ b]
  | .call g τ => by simp [Stmt.subst, fp, List.map_map, Function.comp_def, subst_comp]

/-- **The link check.** Every function's summary covers the footprint of its body, computed with
the summaries: writes by writes, reads by reads or writes. -/
def Valid (S : Nat → Fp) (P : Nat → Stmt) : Prop :=
  ∀ f, covers (S f).2 (fp S (P f)).2 ∧ covers ((S f).1 ++ (S f).2) (fp S (P f)).1

/-- Reads and writes together: a statement may leave a place it might write unchanged, so its
result there depends on the old value. -/
def touched (S : Nat → Fp) (s : Stmt) : List Pl := (fp S s).1 ++ (fp S s).2

/-- $$\mathrm{Valid}(S, P) \implies \mathrm{covers}\bigl(W_S(\mathrm{call}\ f\ \sigma),\ W_S(P_f[\sigma])\bigr) \wedge \mathrm{covers}\bigl(T_S(\mathrm{call}\ f\ \sigma),\ T_S(P_f[\sigma])\bigr)$$ -/
theorem call_covers {S : Nat → Fp} {P : Nat → Stmt} (hv : Valid S P) (f : Nat) (σ : Nat → Pl) :
    covers (fp S (.call f σ)).2 (fp S ((P f).subst σ)).2 ∧
    covers (touched S (.call f σ)) (touched S ((P f).subst σ)) := by
  obtain ⟨hw, hr⟩ := hv f
  refine ⟨?_, ?_⟩
  · rw [fp_subst]; exact covers_map σ hw
  · intro y hy
    simp only [touched, fp_subst, List.mem_append] at hy
    have hrw : covers (((S f).1 ++ (S f).2).map (subst σ)) ((fp S (P f)).1.map (subst σ)) :=
      covers_map σ hr
    have hww : covers ((S f).2.map (subst σ)) ((fp S (P f)).2.map (subst σ)) := covers_map σ hw
    rcases hy with hy | hy
    · obtain ⟨x, hx, h⟩ := hrw y hy
      exact ⟨x, by simpa [touched, fp, List.map_append] using hx, h⟩
    · obtain ⟨x, hx, h⟩ := hww y hy
      exact ⟨x, by simp [touched, fp]; exact Or.inr (by simpa using hx), h⟩

/-- Membership in a footprint that is part of another one. -/
macro "mem_tac" : tactic => `(tactic| (
  intro p hp
  simp only [touched, fp, List.mem_append, List.mem_cons] at hp ⊢
  rcases hp with hp | hp <;> simp [hp]))

/-! ## The two theorems -/

variable {S : Nat → Fp} {P : Nat → Stmt}

/-- $$\mathrm{Valid}(S, P) \wedge l \notin {\downarrow}W_S(s) \implies \mathrm{exec}^n_P(s, m)(l) = m(l)$$

**Frame.** A statement changes no place outside its writes. -/
theorem frame (hv : Valid S P) :
    ∀ (n : Nat) (s : Stmt) (m : Mem) (l : Pl), ¬ under (fp S s).2 l → exec P n s m l = m l
  | 0, _, _, _, _ => rfl
  | _ + 1, .nop, _, _, _ => rfl
  | _ + 1, .assign d f a b, m, l, h => by
    have : l ≠ d := fun e => h ⟨d, by simp [fp], by rw [e]; exact pre_refl d⟩
    simp [exec, this]
  | _ + 1, .move d s, m, l, h => by
    have : pre d l = false := by
      cases hp : pre d l
      · rfl
      · exact absurd ⟨d, by simp [fp], hp⟩ h
    simp [exec, this]
  | n + 1, .seq a b, m, l, h => by
    simp only [fp, under_append, not_or] at h
    simp only [exec]
    rw [frame hv n b _ l h.2, frame hv n a m l h.1]
  | n + 1, .ite c t e, m, l, h => by
    simp only [fp, under_append, not_or] at h
    simp only [exec]
    split
    · exact frame hv n t m l h.1
    · exact frame hv n e m l h.2
  | n + 1, .while c b, m, l, h => by
    simp only [exec]
    split
    · rw [frame hv n (.while c b) _ l h, frame hv n b m l h]
    · rfl
  | n + 1, .call f σ, m, l, h => by
    simp only [exec]
    exact frame hv n _ m l (fun h' => h (under_of_covers (call_covers hv f σ).1 h'))

/-- $$\mathrm{Valid}(S, P) \wedge {\downarrow}T_S(s) \subseteq X \wedge m|_X = m'|_X \implies \mathrm{exec}^n_P(s, m)|_X = \mathrm{exec}^n_P(s, m')|_X$$

**Dependence.** Take any set of places `X` containing everything a statement touches. If two
memories agree on `X` before the statement, they agree on `X` after it. -/
theorem dep (hv : Valid S P) :
    ∀ (n : Nat) (s : Stmt) (X : Pl → Prop), (∀ l, under (touched S s) l → X l) →
      ∀ (m m' : Mem), (∀ l, X l → m l = m' l) → ∀ l, X l → exec P n s m l = exec P n s m' l
  | 0, _, _, _, _, _, h, l, hl => h l hl
  | _ + 1, .nop, _, _, _, _, h, l, hl => h l hl
  | _ + 1, .assign d f a b, X, hX, m, m', h, l, hl => by
    have hop : ∀ o : Op, (∀ p ∈ o.reads, X p) → o.eval m = o.eval m' := by
      intro o ho
      cases o with
      | place p => exact h p (ho p (by simp [Op.reads]))
      | const v => rfl
    have hr : ∀ p, p ∈ a.reads ++ b.reads → X p := fun p hp =>
      hX p ⟨p, List.mem_append.2 (Or.inl hp), pre_refl p⟩
    simp only [exec]
    split
    · rw [hop a (fun p hp => hr p (List.mem_append.2 (Or.inl hp))),
        hop b (fun p hp => hr p (List.mem_append.2 (Or.inr hp)))]
    · exact h l hl
  | _ + 1, .move d s, X, hX, m, m', h, l, hl => by
    simp only [exec]
    split
    · exact h _ (hX _ ⟨s, by simp [touched, fp], (pre_iff _ _).2 ⟨rfl, _, rfl⟩⟩)
    · exact h l hl
  | n + 1, .seq a b, X, hX, m, m', h, l, hl => by
    have ha : ∀ l, under (touched S a) l → X l := fun l hl' => hX l (under_mono (by mem_tac) hl')
    have hb : ∀ l, under (touched S b) l → X l := fun l hl' => hX l (under_mono (by mem_tac) hl')
    simp only [exec]
    exact dep hv n b X hb _ _ (dep hv n a X ha m m' h) l hl
  | n + 1, .ite c t e, X, hX, m, m', h, l, hl => by
    have hc : m c = m' c := h c (hX c ⟨c, by simp [touched, fp], pre_refl c⟩)
    have ht : ∀ l, under (touched S t) l → X l := fun l hl' => hX l (under_mono (by mem_tac) hl')
    have he : ∀ l, under (touched S e) l → X l := fun l hl' => hX l (under_mono (by mem_tac) hl')
    simp only [exec, hc]
    split
    · exact dep hv n t X ht m m' h l hl
    · exact dep hv n e X he m m' h l hl
  | n + 1, .while c b, X, hX, m, m', h, l, hl => by
    have hc : m c = m' c := h c (hX c ⟨c, by simp [touched, fp], pre_refl c⟩)
    have hb : ∀ l, under (touched S b) l → X l := fun l hl' => hX l (under_mono (by mem_tac) hl')
    simp only [exec, hc]
    split
    · exact dep hv n (.while c b) X hX _ _ (dep hv n b X hb m m' h) l hl
    · exact h l hl
  | n + 1, .call f σ, X, hX, m, m', h, l, hl => by
    simp only [exec]
    exact dep hv n _ X (fun l h' => hX l (under_of_covers (call_covers hv f σ).2 h')) m m' h l hl

/-! ## Disjoint footprints commute -/

/-- Disjoint footprints, the independence of `Schedule.lean`: neither writes what the other
touches. -/
def Disjoint (S : Nat → Fp) (a b : Stmt) : Prop :=
  (∀ l, under (fp S a).2 l → ¬ under (touched S b) l ∧ ¬ under (fp S b).2 l) ∧
  (∀ l, under (fp S b).2 l → ¬ under (touched S a) l)

/-- $$l \in {\downarrow}W_S(s) \implies l \in {\downarrow}T_S(s)$$ -/
theorem under_touched_of_write {s : Stmt} {l : Pl} (h : under (fp S s).2 l) :
    under (touched S s) l :=
  under_mono (fun _ hp => List.mem_append.2 (Or.inr hp)) h

/-- A statement as a client step of `World.lean`: it sends no request. -/
def toCOp (hv : Valid S P) (n : Nat) (s : Stmt) : COp Pl Int Empty Unit where
  id := 0
  R := under (touched S s)
  W := under (fp S s).2
  req := fun _ => none
  upd := fun _ m => exec P n s m
  frame := fun _ m l h => frame hv n s m l h
  req_dep := fun _ _ _ => rfl
  upd_dep := fun _ m m' h l hl =>
    dep hv n s (under (touched S s)) (fun _ h => h) m m' h l (under_touched_of_write hl)

/-- The statements of the fragment, with fuel `n`, as a system whose independence is disjoint
footprints. Commutation is `upd_comm` of `World.lean`, from `frame` and `dep`. -/
def memSys (hv : Valid S P) (n : Nat) : Sys Mem Stmt where
  run := exec P n
  Ind := Disjoint S
  comm := fun a b h m => upd_comm (toCOp hv n a) (toCOp hv n b) h none none m

/-! ## From the certificate to `hind` -/

/-- A place as the certificate writes it: the local, then the projections. -/
def enc (p : Pl) : Place := p.1 :: p.2

/-- The certificate's node covers a footprint: every written place extends a place the node
writes, every read place one it reads or writes. -/
def CoversNode (nd : Node) (f : Fp) : Prop :=
  (∀ p ∈ f.2, ∃ q ∈ nd.writes, lpre q (enc p) = true) ∧
  (∀ p ∈ f.1, ∃ q ∈ nd.reads ++ nd.writes, lpre q (enc p) = true)

/-- $$a \mathbin{+\!\!+} t = b \mathbin{+\!\!+} t' \implies \mathrm{overlap}_{\mathit{holes}}(a, b) = \mathsf{true}$$ -/
theorem overlap_common (holes : List Nat) :
    ∀ (a b t t' : List Nat), a ++ t = b ++ t' → overlap holes a b = true
  | [], _, _, _, _ => by simp [overlap]
  | _ :: _, [], _, _, _ => by simp [overlap]
  | x :: xs, y :: ys, t, t', h => by
    simp only [List.cons_append, List.cons.injEq] at h
    obtain ⟨rfl, h⟩ := h
    simp [overlap, overlap_common holes xs ys t t' h]

/-- $$\mathrm{lpre}(q, \mathrm{enc}(p)) \wedge p \sqsubseteq l \implies \exists t,\ \mathrm{enc}(l) = q \mathbin{+\!\!+} t$$ -/
theorem enc_ext {q : Place} {p l : Pl} (hq : lpre q (enc p) = true) (hp : pre p l = true) :
    ∃ t, enc l = q ++ t := by
  obtain ⟨t1, h1⟩ := (lpre_iff _ _).1 hq
  obtain ⟨e, t2, h2⟩ := (pre_iff _ _).1 hp
  refine ⟨t1 ++ t2, ?_⟩
  have : enc l = enc p ++ t2 := by simp [enc, e, h2]
  rw [this, h1, List.append_assoc]

/-- $$\mathrm{CoversNode}(nd, f) \wedge l \in {\downarrow}W_f \implies \exists q \in nd.\mathrm{writes},\ \exists t,\ \mathrm{enc}(l) = q \mathbin{+\!\!+} t$$ -/
theorem write_place {nd : Node} {f : Fp} (hc : CoversNode nd f) {l : Pl} (h : under f.2 l) :
    ∃ q ∈ nd.writes, ∃ t, enc l = q ++ t := by
  obtain ⟨p, hp, hpl⟩ := h
  obtain ⟨q, hq, hqp⟩ := hc.1 p hp
  exact ⟨q, hq, enc_ext hqp hpl⟩

/-- $$\mathrm{CoversNode}(nd, f) \wedge l \in {\downarrow}(R_f \mathbin{+\!\!+} W_f) \implies \exists q \in nd.\mathrm{reads} \mathbin{+\!\!+} nd.\mathrm{writes},\ \exists t,\ \mathrm{enc}(l) = q \mathbin{+\!\!+} t$$ -/
theorem touch_place {nd : Node} {f : Fp} (hc : CoversNode nd f) {l : Pl} (h : under (f.1 ++ f.2) l) :
    ∃ q ∈ nd.reads ++ nd.writes, ∃ t, enc l = q ++ t := by
  rcases under_append.1 h with h | h
  · obtain ⟨p, hp, hpl⟩ := h
    obtain ⟨q, hq, hqp⟩ := hc.2 p hp
    exact ⟨q, hq, enc_ext hqp hpl⟩
  · obtain ⟨q, hq, t, ht⟩ := write_place hc h
    exact ⟨q, List.mem_append.2 (Or.inr hq), t, ht⟩

/-- $$a \in xs \wedge b \in ys \wedge a \mathbin{+\!\!+} t = b \mathbin{+\!\!+} t' \implies \mathrm{anyOverlap}_{\mathit{holes}}(xs, ys) = \mathsf{true}$$ -/
theorem anyOverlap_of {holes : List Nat} {xs ys : List Place} {a b : Place} (ha : a ∈ xs)
    (hb : b ∈ ys) {t t' : List Nat} (h : a ++ t = b ++ t') : anyOverlap holes xs ys = true :=
  List.any_eq_true.2 ⟨a, ha, List.any_eq_true.2 ⟨b, hb, overlap_common holes a b t t' h⟩⟩

/-- $$\bigl(\forall i,\ \mathrm{CoversNode}(c_i, \mathrm{fp}_S(st_i))\bigr) \implies \bigl(\forall i\,j,\ c.\mathrm{ind}(i, j) \Rightarrow \mathrm{Disjoint}_S(st_i, st_j)\bigr)$$

**`hind`, proved.** If every certificate node covers its statement's footprint, the
certificate's independence implies disjoint footprints. -/
theorem hind_of_cover {c : Cert} {st : Nat → Stmt}
    (hcov : ∀ i, CoversNode (c.node i) (fp S (st i))) :
    ∀ i j, c.ind i j = true → Disjoint S (st i) (st j) := by
  intro i j h
  simp only [Cert.ind, indep, Bool.and_eq_true, Bool.not_eq_true'] at h
  obtain ⟨h1, h2⟩ := h
  have clash : ∀ {l}, under (fp S (st i)).2 l → under (touched S (st j)) l → False := by
    intro l hw ht
    obtain ⟨a, ha, t, hat⟩ := write_place (hcov i) hw
    obtain ⟨b, hb, t', hbt⟩ := touch_place (hcov j) ht
    have := anyOverlap_of (holes := c.holes) ha hb (hat.symm.trans hbt)
    rw [h1] at this; exact Bool.false_ne_true this
  refine ⟨fun l hw => ⟨fun ht => clash hw ht, fun hw' => clash hw (under_touched_of_write hw')⟩, ?_⟩
  intro l hw ht
  obtain ⟨a, ha, t, hat⟩ := write_place (hcov j) hw
  obtain ⟨b, hb, t', hbt⟩ := touch_place (hcov i) ht
  rcases List.mem_append.1 hb with hb | hb
  · have := anyOverlap_of (holes := c.holes) ha hb (hat.symm.trans hbt)
    rw [h2] at this; exact Bool.false_ne_true this
  · have := anyOverlap_of (holes := c.holes) (ys := (c.node j).reads ++ (c.node j).writes) hb
      (List.mem_append.2 (Or.inr ha))
      (hbt.symm.trans hat)
    rw [h1] at this; exact Bool.false_ne_true this

/-- $$\mathrm{Valid}(S, P) \wedge \bigl(\forall i,\ \mathrm{CoversNode}(c_i, \mathrm{fp}_S(st_i))\bigr) \wedge \mathrm{check}(c) = \mathsf{true} \wedge \mathrm{Execs}(st(c.\mathit{levels}), ms) \implies [\![ \mathrm{flatten}(ms) ]\!]_{\mathrm{mem}}(m) = [\![ st_0, \ldots, st_{n-1} ]\!]_{\mathrm{mem}}(m)$$

**Certified schedules of MIR, with nothing about footprints assumed.** For a program whose
summaries pass the link check, and a certificate whose nodes cover the statements' footprints, if
`check` accepts the certificate then every run of its schedule, in any interleaving of each
level, ends with the same memory as the statements run in order. -/
theorem mir_schedule_sound (hv : Valid S P) (n : Nat) (c : Cert) (st : Nat → Stmt)
    (hcov : ∀ i, CoversNode (c.node i) (fp S (st i))) (hc : check c = true)
    {ms : List (List Stmt)} (hx : Execs (c.levels.map (List.map (List.map st))) ms) (m : Mem) :
    (memSys hv n).runAll ms.flatten m = (memSys hv n).runAll ((List.range c.nodes.length).map st) m :=
  check_sound (memSys hv n) c st (hind_of_cover hcov) hc hx m

/-! ## The link check, decided -/

/-- A finite program: each function's body and its summary (from its crate's summary file).
Functions past the end are empty. -/
structure Program where
  fns : List (Stmt × Fp)

def Program.body (p : Program) (f : Nat) : Stmt := (p.fns.getD f (.nop, ([], []))).1
def Program.sum (p : Program) (f : Nat) : Fp := (p.fns.getD f (.nop, ([], []))).2

def coversB (xs ys : List Pl) : Bool := ys.all fun y => xs.any fun x => pre x y

/-- $$\mathrm{coversB}(xs, ys) = \mathsf{true} \implies \mathrm{covers}(xs, ys)$$ -/
theorem covers_of_coversB {xs ys : List Pl} (h : coversB xs ys = true) : covers xs ys := by
  intro y hy
  have := List.all_eq_true.1 h y hy
  obtain ⟨x, hx, hxy⟩ := List.any_eq_true.1 this
  exact ⟨x, hx, hxy⟩

/-- The link step's check: for every function, the summary covers the body's footprint. -/
def Program.validB (p : Program) : Bool :=
  p.fns.all fun e => coversB e.2.2 (fp p.sum e.1).2 && coversB (e.2.1 ++ e.2.2) (fp p.sum e.1).1

/-- $$p.\mathrm{validB} = \mathsf{true} \implies \mathrm{Valid}(p.\mathrm{sum}, p.\mathrm{body})$$ -/
theorem Program.valid (p : Program) (h : p.validB = true) : Valid p.sum p.body := by
  intro f
  simp only [Program.body, Program.sum, List.getD_eq_getElem?_getD]
  cases he : p.fns[f]? with
  | none => exact ⟨fun _ h => by simp [fp] at h, fun _ h => by simp [fp] at h⟩
  | some e =>
    have hm : e ∈ p.fns := List.mem_of_getElem? he
    have := List.all_eq_true.1 h e hm
    simp only [Bool.and_eq_true] at this
    simp only [Option.getD_some]
    exact ⟨covers_of_coversB this.1, covers_of_coversB this.2⟩

/-! ## Example: a recursive callee, called on two fields -/

/-- `fn drain(_0: &mut u64, _1: &mut u64) { if *_0 != 0 { *_1 += 1; *_0 -= 1; drain(_0, _1) } }`.
It is recursive, so its summary is checked against its own body. -/
def drain : Stmt :=
  .ite (0, []) (.seq (.assign (1, []) (· + ·) (.place (1, [])) (.const 1))
    (.seq (.assign (0, []) (· - ·) (.place (0, [])) (.const 1)) (.call 0 fun k => (k, []))))
    .nop

def drainSum : Fp := ([], [(0, []), (1, [])])

def prog : Program := ⟨[(drain, drainSum)]⟩

/-- $$\mathtt{prog}.\mathrm{validB} = \mathsf{true}$$ -/
theorem prog_valid : prog.validB = true := by decide

/-- $$\mathrm{validB}\bigl(\mathtt{drain} : R = \emptyset,\ W = \{\_0\}\bigr) = \mathsf{false}$$

A summary that forgets the write to `_1` is rejected by the link check. -/
theorem prog_wrong_invalid : (Program.validB ⟨[(drain, ([], [(0, [])]))]⟩) = false := by decide

/-- The caller: `drain(&mut s.a, &mut s.b)` and `drain(&mut s.c, &mut s.d)`, with `s` local 5.
The certificate puts them in one level; it covers the footprints, and `check` accepts it. -/
def callers : Nat → Stmt
  | 0 => .call 0 fun k => (5, [k])
  | 1 => .call 0 fun k => (5, [k + 2])
  | _ => .nop

def callersCert : Cert where
  nodes := [⟨[], [[5, 0], [5, 1]]⟩, ⟨[], [[5, 2], [5, 3]]⟩]
  levels := [[[0], [1]]]

/-- $$\mathrm{check}(\mathtt{callersCert}) = \mathsf{true}$$ -/
theorem callersCert_ok : check callersCert = true := by decide

/-- $$\forall i,\ \mathrm{CoversNode}\bigl(\mathtt{callersCert}_i,\ \mathrm{fp}(\mathtt{callers}_i)\bigr)$$ -/
theorem callers_cover : ∀ i, CoversNode (callersCert.node i) (fp prog.sum (callers i)) := by
  intro i
  match i with
  | 0 => unfold CoversNode; decide
  | 1 => unfold CoversNode; decide
  | _ + 2 => exact ⟨fun _ h => by simp [callers, fp] at h, fun _ h => by simp [callers, fp] at h⟩

/-- $$\mathrm{Execs}(\mathtt{callers}(\mathtt{callersCert}.\mathit{levels}), ms) \implies [\![ \mathrm{flatten}(ms) ]\!]_{\mathrm{mem}}(m) = [\![ \mathtt{callers}_0, \mathtt{callers}_1 ]\!]_{\mathrm{mem}}(m) \quad \text{for every fuel}$$

Both calls in parallel equal both in order, for every fuel, with no hypothesis left. -/
theorem callers_sound (n : Nat) {ms : List (List Stmt)}
    (hx : Execs (callersCert.levels.map (List.map (List.map callers))) ms) (m : Mem) :
    (memSys (prog.valid prog_valid) n).runAll ms.flatten m =
      (memSys (prog.valid prog_valid) n).runAll [callers 0, callers 1] m :=
  mir_schedule_sound (prog.valid prog_valid) n callersCert callers callers_cover callersCert_ok hx m

end PsyncFootprint
