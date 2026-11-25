/-!
# psync's schedule and the outside world

`Schedule.lean` proves that psync's schedule preserves the *client's* state. A real program also
talks to servers, and a server may change its state even on a read: a `GET` may bump a rate-limit
counter, refresh a session or fill a cache. No client-side analysis (tokens, MIR, anything) can see
that. So no tool can promise equivalence outright; what it can promise is equivalence *relative to
a model of the server*. This file makes that model explicit and proves the schedule against it.

1. **A reduction kernel** (section `Kernel`): `Schedule.lean`'s theorems restated for any state
   type and any independence relation that implies commutation. Nothing in their proofs used more.
2. **The world** (section `World`): a client store, a server given as an abstract labelled
   transition system (`Server`: each request label moves the server to a new state and yields a
   response), and a log of the response every call received. A **commutativity specification**
   `spec` says which requests commute; `Sound` says it is true of the server. Independence of two
   statements is then: disjoint client footprints, distinct calls, and requests the spec says
   commute.
3. **Theorem** `world_levels_correct`: if the spec is sound, every concurrent execution of psync's
   schedule ends with the same client state, the same **server state**, and the same **response
   for every call** as the sequential program.
4. **Reads that write** (section `Example`): a server whose every request increments a counter.
   Two lookups commute (the counter ends the same and no response mentions it); a lookup and a
   read of the counter do not, and the spec in the example says so.

The spec is the only assumption about the server. It is explicit, and it is testable: a
server-side log in which two concurrent requests were applied in an order whose result differs is
a counterexample to the spec.

    lean verify/World.lean      # prints nothing when every proof checks
-/

namespace PsyncWorld

/-! ## Kernel: reduction for any state and any commuting relation -/

section Kernel

/-- A system: steps `A` acting on states `S`, with an independence relation under which steps
commute. -/
structure Sys (S A : Type) where
  run : A → S → S
  Ind : A → A → Prop
  comm : ∀ a b, Ind a b → ∀ s, run b (run a s) = run a (run b s)

variable {S A : Type}

def Sys.runAll (T : Sys S A) : List A → S → S
  | [], s => s
  | a :: as, s => T.runAll as (T.run a s)

theorem Sys.runAll_append (T : Sys S A) (xs ys : List A) (s : S) :
    T.runAll (xs ++ ys) s = T.runAll ys (T.runAll xs s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih => exact ih _

theorem Sys.commute_list (T : Sys S A) (y : A) (xs : List A) (h : ∀ x ∈ xs, T.Ind x y) (s : S) :
    T.runAll xs (T.run y s) = T.run y (T.runAll xs s) := by
  induction xs generalizing s with
  | nil => rfl
  | cons x xs ih =>
    show T.runAll xs (T.run x (T.run y s)) = T.run y (T.runAll xs (T.run x s))
    rw [← T.comm x y (h x (List.mem_cons_self ..)) s]
    exact ih (fun x' hx => h x' (List.mem_cons_of_mem _ hx)) _

inductive Interleave : List A → List A → List A → Prop
  | nil : Interleave [] [] []
  | left {x a b m} : Interleave a b m → Interleave (x :: a) b (x :: m)
  | right {y a b m} : Interleave a b m → Interleave a (y :: b) (y :: m)

theorem Sys.interleave_correct (T : Sys S A) {a b m : List A}
    (hab : ∀ x ∈ a, ∀ y ∈ b, T.Ind x y) (hm : Interleave a b m) (s : S) :
    T.runAll m s = T.runAll (a ++ b) s := by
  induction hm generalizing s with
  | nil => rfl
  | @left x a b m _ ih => exact ih (fun x' hx y hy => hab x' (List.mem_cons_of_mem _ hx) y hy) _
  | @right y a b m _ ih =>
    show T.runAll m (T.run y s) = T.runAll (a ++ y :: b) s
    rw [ih (fun x hx y' hy => hab x hx y' (List.mem_cons_of_mem _ hy)), T.runAll_append,
      T.runAll_append, T.commute_list y a (fun x hx => hab x hx y (List.mem_cons_self ..))]
    rfl

inductive InterleaveAll : List (List A) → List A → Prop
  | nil : InterleaveAll [] []
  | cons {c cs m r} : InterleaveAll cs r → Interleave c r m → InterleaveAll (c :: cs) m

theorem interleave_mem {a b m : List A} (hi : Interleave a b m) : ∀ z ∈ m, z ∈ a ∨ z ∈ b := by
  induction hi with
  | nil => intro z hz; cases hz
  | @left x a b m _ ih =>
    intro z hz
    cases hz with
    | head => exact Or.inl (List.mem_cons_self ..)
    | tail _ hz => exact (ih z hz).imp (List.mem_cons_of_mem _) id
  | @right y a b m _ ih =>
    intro z hz
    cases hz with
    | head => exact Or.inr (List.mem_cons_self ..)
    | tail _ hz => exact (ih z hz).imp id (List.mem_cons_of_mem _)

theorem interleaveAll_mem {cs : List (List A)} {m : List A}
    (h : InterleaveAll cs m) : ∀ y ∈ m, ∃ c ∈ cs, y ∈ c := by
  induction h with
  | nil => intro y hy; cases hy
  | @cons c cs m r _ hi ih =>
    intro y hy
    rcases interleave_mem hi y hy with h | h
    · exact ⟨c, List.mem_cons_self .., h⟩
    · obtain ⟨c', hc', hy'⟩ := ih y h
      exact ⟨c', List.mem_cons_of_mem _ hc', hy'⟩

def Sys.PairwiseInd (T : Sys S A) : List (List A) → Prop
  | [] => True
  | c :: cs => (∀ c' ∈ cs, ∀ x ∈ c, ∀ y ∈ c', T.Ind x y) ∧ T.PairwiseInd cs

theorem Sys.level_correct (T : Sys S A) {cs : List (List A)} {m : List A}
    (hp : T.PairwiseInd cs) (hm : InterleaveAll cs m) (s : S) :
    T.runAll m s = T.runAll cs.flatten s := by
  induction hm generalizing s with
  | nil => rfl
  | @cons c cs m r hr hi ih =>
    have indep : ∀ x ∈ c, ∀ y ∈ r, T.Ind x y := by
      intro x hx y hy
      obtain ⟨c', hc', hy'⟩ := interleaveAll_mem hr y hy
      exact hp.1 c' hc' x hx y hy'
    rw [T.interleave_correct indep hi, T.runAll_append, List.flatten_cons, T.runAll_append,
      ih hp.2]

inductive Sys.Runs (T : Sys S A) : List (List (List A)) → List (List A) → Prop
  | nil : T.Runs [] []
  | cons {cs m levels ms} :
      T.PairwiseInd cs → InterleaveAll cs m → T.Runs levels ms → T.Runs (cs :: levels) (m :: ms)

/-- Any execution of a schedule of levels equals running every chain in order. -/
theorem Sys.levels_correct (T : Sys S A) {levels : List (List (List A))} {ms : List (List A)}
    (h : T.Runs levels ms) (s : S) :
    T.runAll ms.flatten s = T.runAll (levels.map List.flatten).flatten s := by
  induction h generalizing s with
  | nil => rfl
  | @cons cs m levels ms hp hm _ ih =>
    rw [List.flatten_cons, T.runAll_append, List.map_cons, List.flatten_cons, T.runAll_append,
      T.level_correct hp hm, ih]

inductive Sys.Reorders (T : Sys S A) : List A → List A → Prop
  | nil : T.Reorders [] []
  | cons {x P A' B} : (∀ y ∈ A', T.Ind y x) → T.Reorders P (A' ++ B) →
      T.Reorders (x :: P) (A' ++ x :: B)

/-- Moving steps earlier only past independent ones preserves the result. -/
theorem Sys.reorders_correct (T : Sys S A) {P L : List A} (h : T.Reorders P L) (s : S) :
    T.runAll L s = T.runAll P s := by
  induction h generalizing s with
  | nil => rfl
  | @cons x P A' B hA _ ih =>
    show T.runAll (A' ++ x :: B) s = T.runAll P (T.run x s)
    rw [T.runAll_append]
    show T.runAll B (T.run x (T.runAll A' s)) = T.runAll P (T.run x s)
    rw [← T.commute_list x A' hA, ← T.runAll_append, ih]

end Kernel

/-! ## World: client, server, and the response of every call -/

section World

variable {Loc Val L Rsp σ : Type}

/-- A server as a labelled transition system: a request `l` takes state `s` to `(step l s).1` and
answers `(step l s).2`. Nothing is assumed about which requests change the state: a read that
writes (a counter, a session refresh, a cache fill) is an ordinary transition. -/
abbrev Server (L Rsp σ : Type) := L → σ → σ × Rsp

/-- Requests `a` and `b` commute at the server: in either order, the server ends in the same state
and each request gets the same response. -/
def Commutes (step : Server L Rsp σ) (a b : L) : Prop :=
  ∀ s, (step b (step a s).1).1 = (step a (step b s).1).1 ∧
       (step a (step b s).1).2 = (step a s).2 ∧
       (step b (step a s).1).2 = (step b s).2

/-- A commutativity specification is **sound** for a server when every pair it allows commutes.
This is the one assumption about the outside world. -/
def Sound (step : Server L Rsp σ) (spec : L → L → Prop) : Prop :=
  ∀ a b, spec a b → Commutes step a b

/-- What the program and the world consist of: the client's store, the server's state, and the
response each call (by its id) received. -/
structure World (Loc Val Rsp σ : Type) where
  c : Loc → Val
  s : σ
  log : Nat → Option Rsp

/-- A statement of the client program. It reads `R` and writes `W` in the client. It may send one
request, computed from what it reads (`req`), and then updates the client from the response
(`upd`). `id` names the call in the log. -/
structure COp (Loc Val L Rsp : Type) where
  id : Nat
  R : Loc → Prop
  W : Loc → Prop
  req : (Loc → Val) → Option L
  upd : Option Rsp → (Loc → Val) → (Loc → Val)
  frame : ∀ o c l, ¬ W l → upd o c l = c l
  req_dep : ∀ c c', (∀ l, R l → c l = c' l) → req c = req c'
  upd_dep : ∀ o c c', (∀ l, R l → c l = c' l) → ∀ l, W l → upd o c l = upd o c' l

/-- Record response `r` for call `i`. -/
def record (log : Nat → Option Rsp) (i : Nat) (r : Rsp) : Nat → Option Rsp :=
  fun j => if j = i then some r else log j

def COp.run (step : Server L Rsp σ) (o : COp Loc Val L Rsp) (w : World Loc Val Rsp σ) :
    World Loc Val Rsp σ :=
  match o.req w.c with
  | none => ⟨o.upd none w.c, w.s, w.log⟩
  | some l => ⟨o.upd (some (step l w.s).2) w.c, (step l w.s).1, record w.log o.id (step l w.s).2⟩

/-- Independent relative to `spec`: disjoint client footprints (as in `Schedule.lean`), distinct
calls, and whatever requests the two may send are ones the spec says commute. -/
def CIndep (spec : L → L → Prop) (a b : COp Loc Val L Rsp) : Prop :=
  ((∀ l, a.W l → ¬ b.R l ∧ ¬ b.W l) ∧ (∀ l, b.W l → ¬ a.R l)) ∧ a.id ≠ b.id ∧
  ∀ c c' la lb, a.req c = some la → b.req c' = some lb → spec la lb

/-- `b`'s update does not change what `a` reads, so `a` sends the same request. -/
theorem req_stable (a b : COp Loc Val L Rsp) (h : ∀ l, b.W l → ¬ a.R l) (o : Option Rsp)
    (c : Loc → Val) : a.req (b.upd o c) = a.req c :=
  a.req_dep _ _ (fun l hr => b.frame o c l (fun hw => h l hw hr))

/-- The client updates of two statements with disjoint footprints commute, whatever the
responses (`commute` of `Schedule.lean`). -/
theorem upd_comm (a b : COp Loc Val L Rsp)
    (h : (∀ l, a.W l → ¬ b.R l ∧ ¬ b.W l) ∧ (∀ l, b.W l → ¬ a.R l))
    (oa ob : Option Rsp) (c : Loc → Val) :
    b.upd ob (a.upd oa c) = a.upd oa (b.upd ob c) := by
  funext l
  by_cases ha : a.W l
  · have hb : ¬ b.W l := (h.1 l ha).2
    rw [b.frame _ _ _ hb]
    exact a.upd_dep oa c (b.upd ob c)
      (fun l' hr => (b.frame ob c l' (fun hw => h.2 l' hw hr)).symm) l ha
  · by_cases hb : b.W l
    · rw [a.frame _ _ _ ha]
      exact b.upd_dep ob (a.upd oa c) c
        (fun l' hr => a.frame oa c l' (fun hw => (h.1 l' hw).1 hr)) l hb
    · rw [b.frame _ _ _ hb, a.frame _ _ _ ha, a.frame _ _ _ ha, b.frame _ _ _ hb]

theorem record_comm (log : Nat → Option Rsp) {i j : Nat} (hij : i ≠ j) (ri rj : Rsp) :
    record (record log i ri) j rj = record (record log j rj) i ri := by
  funext k
  unfold record
  by_cases hj : k = j
  · simp [hj, Ne.symm hij]
  · simp [hj]

/-- **Independent statements commute in the world**: same client state, same server state, same
response for every call. The server part is exactly the spec's soundness. -/
theorem world_commute (step : Server L Rsp σ) (spec : L → L → Prop) (hs : Sound step spec)
    (a b : COp Loc Val L Rsp) (h : CIndep spec a b) (w : World Loc Val Rsp σ) :
    b.run step (a.run step w) = a.run step (b.run step w) := by
  obtain ⟨hfp, hid, hreq⟩ := h
  have hba : ∀ o c, b.req (a.upd o c) = b.req c :=
    fun o c => req_stable b a (fun l hw hr => (hfp.1 l hw).1 hr) o c
  have hab : ∀ o c, a.req (b.upd o c) = a.req c := fun o c => req_stable a b hfp.2 o c
  cases hA : a.req w.c with
  | none =>
    cases hB : b.req w.c with
    | none =>
      simp only [COp.run, hA, hB, hab, hba]
      rw [upd_comm a b hfp]
    | some lb =>
      simp only [COp.run, hA, hB, hab, hba]
      rw [upd_comm a b hfp]
  | some la =>
    cases hB : b.req w.c with
    | none =>
      simp only [COp.run, hA, hB, hab, hba]
      rw [upd_comm a b hfp]
    | some lb =>
      have hc := hs la lb (hreq w.c w.c la lb hA hB) w.s
      obtain ⟨hstate, hra, hrb⟩ := hc
      simp only [COp.run, hA, hB, hab, hba, hra, hrb, hstate]
      rw [upd_comm a b hfp, record_comm _ (Ne.symm hid)]

/-- The world as a `Sys`: psync's statements, run against a server whose spec is sound. -/
def worldSys (step : Server L Rsp σ) (spec : L → L → Prop) (hs : Sound step spec) :
    Sys (World Loc Val Rsp σ) (COp Loc Val L Rsp) where
  run := COp.run step
  Ind := CIndep spec
  comm := fun a b h w => world_commute step spec hs a b h w

/-- **The theorem.** If the commutativity specification is sound for the server, then whatever
interleaving each level of psync's schedule takes, the run ends exactly as the sequential program
does: the same client state, the same server state, and the same response for every call. -/
theorem world_levels_correct (step : Server L Rsp σ) (spec : L → L → Prop) (hs : Sound step spec)
    {levels : List (List (List (COp Loc Val L Rsp)))} {ms : List (List (COp Loc Val L Rsp))}
    (h : (worldSys step spec hs).Runs levels ms) (w : World Loc Val Rsp σ) :
    (worldSys step spec hs).runAll ms.flatten w =
      (worldSys step spec hs).runAll (levels.map List.flatten).flatten w :=
  (worldSys step spec hs).levels_correct h w

/-- With no specification at all (`spec := fun _ _ => False`, psync's default for an unknown
service: every two calls to it conflict), the theorem holds for every server: only statements that
send no request, or send none to that server, may overlap. -/
theorem empty_spec_sound (step : Server L Rsp σ) : Sound step (fun _ _ => False) :=
  fun _ _ h => h.elim

end World

/-! ## Example: reads that write -/

section Example

/-- A key-value server that counts every request (a rate limiter): `get k` returns the value and
bumps the counter; `count` returns the counter and bumps it too. -/
inductive Req
  | get (k : Nat)
  | count

structure KV where
  store : Nat → Nat
  hits : Nat

def kv : Server Req Nat KV
  | .get k, s => (⟨s.store, s.hits + 1⟩, s.store k)
  | .count, s => (⟨s.store, s.hits + 1⟩, s.hits)

/-- The spec: two `get`s commute. Nothing involving `count` does, because `count`'s answer sees
every other request: both are "reads", and they still do not commute. -/
def kvSpec : Req → Req → Prop
  | .get _, .get _ => True
  | _, _ => False

theorem kvSpec_sound : Sound kv kvSpec := by
  intro a b h s
  cases a <;> cases b <;> simp_all [kvSpec, kv]

/-- A `get` and a `count` really do not commute: `count`'s answer depends on whether the `get`
came first. A spec that allowed them would be unsound, and the theorem would not apply. -/
theorem get_count_not_commute : ¬ Commutes kv (.get 0) .count := by
  intro h
  have := (h ⟨fun _ => 0, 0⟩).2.2
  simp [kv] at this

/-! ### psync's built-in rule, as a spec

Without user input, psync classifies each call by its name (`effects.rs`) or, with compiler facts,
by its body: a service, an access (read, write, or ordered write) and a target when one is known.
Two calls commute unless one writes, they are on the same service, and their targets may be the
same (an unknown target may be any). That rule *is* a commutativity specification, and the
theorem above holds exactly when it is sound for the servers involved. Its weakest clause is
"two reads always commute": a read that changes server state breaks it. -/

inductive Access
  | read
  | write
  | ordered

/-- How psync describes a call: service, access, and target (`none`: unknown). -/
structure Eff where
  svc : Nat
  acc : Access
  target : Option Nat

/-- psync's conflict rule, negated: when psync lets two calls overlap. -/
def psyncSpec (a b : Eff) : Prop :=
  a.svc ≠ b.svc ∨ (a.acc = .read ∧ b.acc = .read) ∨
  (¬ (a.acc = .ordered ∧ b.acc = .ordered) ∧ ∃ x y, a.target = some x ∧ b.target = some y ∧ x ≠ y)

/-- How psync's names classify the example's requests: `get_value(k)` and `fetch_count()` are both
reads of the one service (`get`, `fetch` are read verbs); only `get` has a known target. -/
def kvEff : Req → Eff
  | .get k => ⟨0, .read, some k⟩
  | .count => ⟨0, .read, none⟩

/-- **The professor's point, proved.** For a server whose reads change its state, psync's
built-in rule is not sound: it lets a `get` and a `count` overlap, and they do not commute. So
the guarantee is relative to the spec, and a counting server needs a spec that says so (here
`kvSpec`, or `#[psync::seq]`, or `effects(write(fetch_count))`). -/
theorem psync_rule_unsound_for_counters : ¬ Sound kv (fun a b => psyncSpec (kvEff a) (kvEff b)) :=
  fun h => get_count_not_commute (h (.get 0) .count (Or.inr (Or.inl ⟨rfl, rfl⟩)))

/-- And it is sound when the server's reads do not change what other requests see: two `get`s. -/
theorem psync_rule_sound_for_gets (k k' : Nat) : Commutes kv (.get k) (.get k') :=
  kvSpec_sound _ _ trivial

end Example

end PsyncWorld
