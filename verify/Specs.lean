import Footprint

/-!
# Commutativity specifications for HTTP, S3 and the Kubernetes API

`World.lean` proves psync's schedule correct against any server, relative to a commutativity
specification that is `Sound` for that server. This file gives three servers as explicit models and
proves a specification sound for each, together with the places where a plausible rule fails.

* **HTTP** (RFC 9110). Resources on a tree of paths. `GET`, `HEAD` and listing a collection are
  *safe* (§9.2.1: they do not change the resource state); `PUT` and `DELETE` change their target
  only. The spec: two safe requests commute, and so do two requests whose targets are apart
  (neither is a prefix of the other, psync's rule for paths). *Idempotent is not commutative:*
  two identical `PUT`s leave the same state, but the first answers `201 Created` and the second
  `200 OK`, so they do not commute (`http_put_twice_not_commute`).
* **S3** (without versioning; strongly consistent, as S3 is since 2020). The same, and since an
  object's ETag depends only on its body and `DeleteObject` answers `204` whether or not the key
  existed, two identical `PUT`s commute, and so do two identical `DELETE`s.
* **Kubernetes.** Every write gets the next value of one global counter, the `resourceVersion`,
  and returns it. So two writes to *different* objects do not commute: their resourceVersions
  depend on the order (`kube_writes_not_commute`). They commute for a client that does not use
  resourceVersion values (`kube_commutes_up_to_rv`): the server with resourceVersions erased is a
  simulation of the real one, and the spec is sound for it.

The models are the stated assumptions: one copy of the state, applied request by request
(linearizable), no versioning, lifecycle rules or admission webhooks, and `POST` with an effect the
model does not fix (`HReq.post`), which no spec lets commute with anything.

## Notation used in the theorem statements

* $\mathrm{Commutes}(T, a, b)$: in every state, $a;b$ and $b;a$ end in the same server state and
  give each request the response it gets when run first (`World.lean`).
* $\mathrm{Sound}(T, \mathit{spec})$: every pair the spec allows commutes.
* $p \sqsubseteq q$: path $p$ is a prefix of $q$; $p \mathrel{\#} q$ (`apart`): neither is a
  prefix of the other.
* $\mathrm{erase}$: a Kubernetes state or response with every resourceVersion removed.
-/

namespace PsyncSpecs

open PsyncWorld PsyncFootprint

/-! ## Paths -/

/-- A path: interned segments (`/a/b` is `[a, b]`). -/
abbrev Path := List Nat

/-- Neither path is a prefix of the other, so no request on one can see or change the other. -/
def apart (p q : Path) : Prop := lpre p q = false ∧ lpre q p = false

theorem lpre_self (p : Path) : lpre p p = true := (lpre_iff p p).2 ⟨[], by simp⟩

/-- $$p \mathrel{\#} q \implies p \neq q$$ -/
theorem apart_ne {p q : Path} (h : apart p q) : p ≠ q := by
  intro e; subst e; have := h.1; rw [lpre_self] at this; exact Bool.noConfusion this

/-- $$p \mathrel{\#} q \implies q \mathrel{\#} p$$ -/
theorem apart_symm {p q : Path} (h : apart p q) : apart q p := ⟨h.2, h.1⟩

/-- Set `k` to `v`. -/
def upd {α β : Type} [DecidableEq α] (f : α → β) (k : α) (v : β) : α → β :=
  fun x => if x = k then v else f x

/-- $$p \neq q \implies f[p \mapsto a][q \mapsto b] = f[q \mapsto b][p \mapsto a]$$ -/
theorem upd_comm {α β : Type} [DecidableEq α] {f : α → β} {p q : α} (h : p ≠ q) (a b : β) :
    upd (upd f p a) q b = upd (upd f q b) p a := by
  funext x; unfold upd
  by_cases hq : x = q
  · subst hq; simp [Ne.symm h]
  · simp [hq]

/-- $$q \neq p \implies f[p \mapsto a](q) = f(q)$$ -/
theorem upd_ne {α β : Type} [DecidableEq α] {f : α → β} {p q : α} (h : q ≠ p) (a : β) :
    upd f p a q = f q := by simp [upd, h]

/-- $$f[p \mapsto a][p \mapsto b] = f[p \mapsto b]$$ -/
theorem upd_upd {α β : Type} [DecidableEq α] (f : α → β) (p : α) (a b : β) :
    upd (upd f p a) p b = upd f p b := by
  funext x; unfold upd; by_cases h : x = p <;> simp [h]

/-- The entries at or below `p`: what listing the collection `p` returns. -/
def below {β : Type} (f : Path → Option β) (p : Path) : Path → Option β :=
  fun x => if lpre p x then f x else none

/-- $$p \not\sqsubseteq q \implies \mathrm{below}(f[q \mapsto v], p) = \mathrm{below}(f, p)$$ -/
theorem below_upd {β : Type} {f : Path → Option β} {p q : Path} (h : lpre p q = false)
    (v : Option β) : below (upd f q v) p = below f p := by
  funext x; unfold below
  by_cases hx : lpre p x = true
  · have : x ≠ q := by intro e; subst e; rw [hx] at h; exact Bool.noConfusion h
    simp [hx, upd_ne this]
  · simp [hx]

/-! ## HTTP -/

/-- An HTTP request. `post p` has an effect the server decides (`HServer.post`). -/
inductive HReq
  | get (p : Path)
  | head (p : Path)
  | list (p : Path)
  | put (p : Path) (body : Nat)
  | delete (p : Path)
  | post (p : Path)

/-- A response: a body or its absence (`404`), a listing, or whether the target existed before
(`200`/`204` against `201`/`404`). -/
inductive HRsp
  | body (b : Option Nat)
  | exists_ (e : Bool)
  | listing (l : Path → Option Nat)
  | other (n : Nat)

/-- The server's state: a body per path. -/
abbrev HState := Path → Option Nat

/-- An HTTP server, given what `POST` does. Everything else is RFC 9110's meaning. -/
def http (post : Path → HState → HState × Nat) : Server HReq HRsp HState
  | .get p, s => (s, .body (s p))
  | .head p, s => (s, .exists_ (s p).isSome)
  | .list p, s => (s, .listing (below s p))
  | .put p b, s => (upd s p (some b), .exists_ (s p).isSome)
  | .delete p, s => (upd s p none, .exists_ (s p).isSome)
  | .post p, s => let r := post p s; (r.1, .other r.2)

/-- Safe methods (RFC 9110 §9.2.1). -/
def HReq.safe : HReq → Prop
  | .get _ | .head _ | .list _ => True
  | _ => False

/-- The path a request is about. -/
def HReq.tgt : HReq → Path
  | .get p | .head p | .list p | .put p _ | .delete p | .post p => p

def HReq.isPost : HReq → Prop
  | .post _ => True
  | _ => False

/-- The spec: two safe requests commute; so do two requests other than `POST` on paths apart. -/
def httpSpec (a b : HReq) : Prop :=
  (a.safe ∧ b.safe) ∨ (¬ a.isPost ∧ ¬ b.isPost ∧ apart a.tgt b.tgt)

/-- $$\forall\, \mathit{post},\ \mathrm{Sound}(\mathrm{http}(\mathit{post}), \mathrm{httpSpec})$$

**The HTTP spec is sound**, whatever `POST` does. -/
theorem httpSpec_sound (post : Path → HState → HState × Nat) : Sound (http post) httpSpec := by
  intro a b h s
  rcases h with ⟨ha, hb⟩ | ⟨ha, hb, hab⟩
  · cases a <;> cases b <;> simp_all [HReq.safe, http]
  · have hne := apart_ne hab
    have hne' := Ne.symm hne
    cases a <;> cases b <;>
      simp_all [HReq.isPost, HReq.tgt, http, upd_comm, upd_ne]
    all_goals first | exact below_upd hab.1 _ | exact below_upd hab.2 _

/-- $$\neg\,\mathrm{Commutes}(\mathrm{http}(\mathit{post}), \mathrm{PUT}\,p\,b, \mathrm{PUT}\,p\,b)$$

**Idempotent is not commutative.** Two identical `PUT`s end in the same state, but the second
answers "existed" (`200`) where the first, run alone on an empty server, answers `201`. -/
theorem http_put_twice_not_commute (post : Path → HState → HState × Nat) (p : Path) (b : Nat) :
    ¬ Commutes (http post) (.put p b) (.put p b) := by
  intro h
  have := (h (fun _ => none)).2.1
  simp [http, upd] at this

/-! ## S3 -/

/-- An S3 request on keys (paths). `list p` is `ListObjectsV2` with prefix `p`. -/
inductive S3Req
  | get (k : Path)
  | head (k : Path)
  | list (p : Path)
  | put (k : Path) (body : Nat)
  | delete (k : Path)
  deriving DecidableEq

/-- A response. `PutObject` returns the ETag, a hash of the body (here the body itself);
`DeleteObject` returns `204` whether the key existed or not. -/
inductive S3Rsp
  | body (b : Option Nat)
  | found (e : Bool)
  | listing (l : Path → Option Nat)
  | etag (e : Nat)
  | noContent

def s3 : Server S3Req S3Rsp HState
  | .get k, s => (s, .body (s k))
  | .head k, s => (s, .found (s k).isSome)
  | .list p, s => (s, .listing (below s p))
  | .put k b, s => (upd s k (some b), .etag b)
  | .delete k, s => (upd s k none, .noContent)

def S3Req.safe : S3Req → Prop
  | .get _ | .head _ | .list _ => True
  | _ => False

def S3Req.tgt : S3Req → Path
  | .get k | .head k | .list k | .put k _ | .delete k => k

/-- The spec: two reads; two requests on keys apart; or the same `PUT` or `DELETE` twice. -/
def s3Spec (a b : S3Req) : Prop :=
  (a.safe ∧ b.safe) ∨ apart a.tgt b.tgt ∨ (a = b ∧ ¬ a.safe)

/-- $$\mathrm{Sound}(\mathrm{s3}, \mathrm{s3Spec})$$

**The S3 spec is sound**, including identical `PUT`s and identical `DELETE`s. -/
theorem s3Spec_sound : Sound s3 s3Spec := by
  intro a b h s
  rcases h with ⟨ha, hb⟩ | hab | ⟨rfl, ha⟩
  · cases a <;> cases b <;> simp_all [S3Req.safe, s3]
  · have hne := apart_ne hab
    have hne' := Ne.symm hne
    cases a <;> cases b <;> simp_all [S3Req.tgt, s3, upd_comm, upd_ne]
    all_goals first | exact below_upd hab.1 _ | exact below_upd hab.2 _
  · cases a <;> simp_all [S3Req.safe, s3]

/-- $$b \neq b' \implies \neg\,\mathrm{Commutes}(\mathrm{s3}, \mathrm{PUT}\,k\,b, \mathrm{PUT}\,k\,b')$$

Two `PUT`s of different bodies to one key do not commute: the last one wins. -/
theorem s3_puts_differ_not_commute (k : Path) {b b' : Nat} (h : b ≠ b') :
    ¬ Commutes s3 (.put k b) (.put k b') := by
  intro hc
  have := congrFun (hc (fun _ => none)).1 k
  simp [s3, upd] at this
  exact h this.symm

/-! ## Kubernetes -/

/-- An object: namespace and name. -/
abbrev KKey := Nat × Nat

/-- The API server's state: each object's spec and resourceVersion, and the global counter. -/
structure KState where
  objs : KKey → Option (Nat × Nat)
  rev : Nat

inductive KReq
  | get (k : KKey)
  | list (ns : Nat)
  | apply (k : KKey) (v : Nat)
  | delete (k : KKey)

/-- Responses carry resourceVersions: the object's, the list's, or the write's. -/
inductive KRsp
  | obj (o : Option (Nat × Nat))
  | objs (l : KKey → Option (Nat × Nat)) (rv : Nat)
  | rv (r : Nat)

def inNs {β : Type} (f : KKey → Option β) (ns : Nat) : KKey → Option β :=
  fun k => if k.1 = ns then f k else none

/-- Every write takes the next resourceVersion. -/
def kube : Server KReq KRsp KState
  | .get k, s => (s, .obj (s.objs k))
  | .list ns, s => (s, .objs (inNs s.objs ns) s.rev)
  | .apply k v, s => (⟨upd s.objs k (some (v, s.rev + 1)), s.rev + 1⟩, .rv (s.rev + 1))
  | .delete k, s => (⟨upd s.objs k none, s.rev + 1⟩, .rv (s.rev + 1))

def KReq.read : KReq → Prop
  | .get _ | .list _ => True
  | _ => False

/-- $$\mathrm{Sound}(\mathrm{kube}, \mathrm{both\ reads})$$

Two reads commute, exactly. -/
theorem kube_reads_sound : Sound kube (fun a b => a.read ∧ b.read) := by
  intro a b ⟨ha, hb⟩ s
  cases a <;> cases b <;> simp_all [KReq.read, kube]

/-- $$\neg\,\mathrm{Commutes}(\mathrm{kube}, \mathrm{apply}\,(0,0)\,1, \mathrm{apply}\,(0,1)\,1)$$

**Writes to different objects do not commute**: whichever runs first gets the lower
resourceVersion. psync's built-in rule (different targets commute) is therefore unsound for a
client that reads resourceVersions. -/
theorem kube_writes_not_commute : ¬ Commutes kube (.apply (0, 0) 1) (.apply (0, 1) 1) := by
  intro h
  have := (h ⟨fun _ => none, 0⟩).2.1
  simp [kube] at this

/-! ### Without resourceVersions -/

/-- What a client sees that ignores resourceVersions. -/
inductive ORsp
  | obj (o : Option Nat)
  | objs (l : KKey → Option Nat)
  | done

def erase (s : KState) : KKey → Option Nat := fun k => (s.objs k).map Prod.fst

def eraseR : KRsp → ORsp
  | .obj o => .obj (o.map Prod.fst)
  | .objs l _ => .objs (fun k => (l k).map Prod.fst)
  | .rv _ => .done

/-- The API server without resourceVersions: a key-value store with namespaces. -/
def kubeObs : Server KReq ORsp (KKey → Option Nat)
  | .get k, s => (s, .obj (s k))
  | .list ns, s => (s, .objs (inNs s ns))
  | .apply k v, s => (upd s k (some v), .done)
  | .delete k, s => (upd s k none, .done)

/-- $$\mathrm{erase}(\mathrm{kube}(l, s)) = \mathrm{kubeObs}(l, \mathrm{erase}(s))$$

**Simulation.** Erasing resourceVersions commutes with every request, in the state and in the
response. -/
theorem kube_simulation (l : KReq) (s : KState) :
    erase (kube l s).1 = (kubeObs l (erase s)).1 ∧
    eraseR (kube l s).2 = (kubeObs l (erase s)).2 := by
  cases l with
  | get k => exact ⟨rfl, rfl⟩
  | list ns =>
    refine ⟨rfl, ?_⟩
    simp only [kube, kubeObs, eraseR]
    congr 1; funext k; simp only [inNs, erase]; split <;> rfl
  | apply k v =>
    refine ⟨?_, rfl⟩
    funext x; simp only [kube, kubeObs, erase, upd]; split <;> rfl
  | delete k =>
    refine ⟨?_, rfl⟩
    funext x; simp only [kube, kubeObs, erase, upd]; split <;> rfl

def KReq.key : KReq → Option KKey
  | .apply k _ | .delete k | .get k => some k
  | .list _ => none

def KReq.ns : KReq → Nat
  | .apply k _ | .delete k | .get k => k.1
  | .list ns => ns

/-- The spec for clients that ignore resourceVersions: two reads, or two requests that cannot see
each other: different objects, and a listing never in the namespace of a write. -/
def kubeSpec (a b : KReq) : Prop :=
  (a.read ∧ b.read) ∨
  (∃ k k', a.key = some k ∧ b.key = some k' ∧ k ≠ k') ∨
  (a.key = none ∧ b.read) ∨ (a.read ∧ b.key = none) ∨
  (a.key = none ∧ ¬ b.read ∧ a.ns ≠ b.ns) ∨ (¬ a.read ∧ b.key = none ∧ a.ns ≠ b.ns)

theorem inNs_upd {β : Type} {f : KKey → Option β} {ns : Nat} {k : KKey} (h : ns ≠ k.1)
    (v : Option β) : inNs (upd f k v) ns = inNs f ns := by
  funext x; unfold inNs
  by_cases hx : x.1 = ns
  · have : x ≠ k := by intro e; subst e; exact h hx.symm
    simp [hx, upd_ne this]
  · simp [hx]

/-- $$\mathrm{Sound}(\mathrm{kubeObs}, \mathrm{kubeSpec})$$ -/
theorem kubeSpec_sound : Sound kubeObs kubeSpec := by
  intro a b h s
  rcases h with ⟨ha, hb⟩ | ⟨k, k', hk, hk', hne⟩ | ⟨ha, hb⟩ | ⟨ha, hb⟩ | ⟨ha, hb, hn⟩ | ⟨ha, hb, hn⟩
  · cases a <;> cases b <;> simp_all [KReq.read, kubeObs]
  · have hne' := Ne.symm hne
    cases a <;> cases b <;> simp_all [KReq.key, kubeObs, upd_comm, upd_ne]
  · cases a <;> cases b <;> simp_all [KReq.key, KReq.read, kubeObs]
  · cases a <;> cases b <;> simp_all [KReq.key, KReq.read, kubeObs]
  · cases a <;> cases b <;> simp_all [KReq.key, KReq.read, KReq.ns, kubeObs, inNs_upd]
  · cases a <;> cases b <;>
      simp_all [KReq.key, KReq.read, KReq.ns, kubeObs]
    all_goals exact inNs_upd (fun e => hn e.symm) _

/-- $$\mathrm{kubeSpec}(a, b) \implies \forall s,\ \mathrm{erase}(s \cdot a \cdot b) =
\mathrm{erase}(s \cdot b \cdot a) \,\wedge\, \text{responses agree after erasing}$$

**Kubernetes requests the spec allows commute up to resourceVersions**: in either order, the
objects end the same, and every response is the same as when the request runs first, once
resourceVersions are erased. -/
theorem kube_commutes_up_to_rv {a b : KReq} (h : kubeSpec a b) (s : KState) :
    erase (kube b (kube a s).1).1 = erase (kube a (kube b s).1).1 ∧
    eraseR (kube a (kube b s).1).2 = eraseR (kube a s).2 ∧
    eraseR (kube b (kube a s).1).2 = eraseR (kube b s).2 := by
  obtain ⟨c1, c2, c3⟩ := kubeSpec_sound a b h (erase s)
  have sa := kube_simulation a s
  have sb := kube_simulation b s
  have sab := kube_simulation b (kube a s).1
  have sba := kube_simulation a (kube b s).1
  refine ⟨?_, ?_, ?_⟩
  · rw [sab.1, sba.1, sa.1, sb.1]; exact c1
  · rw [sba.2, sb.1, sa.2]; exact c2
  · rw [sab.2, sa.1, sb.2]; exact c3

end PsyncSpecs
