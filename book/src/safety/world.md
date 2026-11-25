# What psync guarantees about the outside world

A fair objection to everything in this part: *the compiler sees only the client.* A server may
change its state even on a read. A `GET` may bump a rate-limit counter, refresh a session or fill a
cache, so two "reads" may not commute at the server, and no client-side analysis can tell, whether
it reads tokens, MIR, or anything else. So what can psync guarantee about concurrent accesses?

The objection is right that nothing can be guaranteed *outright*. It is wrong that nothing can be
guaranteed. Every correct reordering, by a person or a tool, is correct *relative to a model of the
outside world*. psync's contribution is to make that model explicit, prove its schedules against
it, and make it testable. This chapter states the guarantee precisely. The proofs are in
`verify/World.lean`, checked by Lean in CI.

## The guarantee, in four layers

| Layer | What psync knows | From | Strength |
|---|---|---|---|
| client memory | no two concurrent branches race on a variable; borrows end before moves | Rust's ownership types, checked by rustc on the generated code | **sound**: rustc rejects the code otherwise |
| client I/O footprint | which files, services and processes each branch can reach, through every callee and every `dyn` implementation | MIR summaries and link-time class-hierarchy analysis ([`cargo psync check`](./check.md)) | sound up to stated gaps: FFI, function pointers, callees it marks opaque |
| the outside world | which pairs of requests commute | a **commutativity specification**: psync's built-in rule, or one you give | **assumed**, but explicit and testable |
| composition | the schedule behaves like the sequential program | `world_levels_correct`, in Lean | **machine-checked** |

The first row is what a tool for a language without ownership types cannot build soundly, and it
is why the JavaScript tools in this space describe themselves as unsound.

## The model

`World.lean` describes three things.

- **The server** is a *labelled transition system*: a function `step` that takes a request and the
  server's state to a new state and a response. Nothing is assumed about which requests change
  the state. A read that writes, such as a counter, is an ordinary transition.
- **The client program** is psync's statements. Each one reads and writes client locations, as in
  [Proofs](../foundations/proofs.md). It may send one request, computed from what it reads, and
  update the client from the response.
- **The world** is the client's store, the server's state, and the response each call received.

Two requests **commute** at the server when, in either order, the server ends in the same state
*and* each gets the same response. A **specification** is a relation on requests, and it is
**sound** for a server when every pair it allows really commutes.

Two statements are **independent relative to the spec** when their client footprints are
disjoint (no edge between them) and any requests they send are ones the spec allows to commute.

## The theorem

> **`world_levels_correct`.** If the specification is sound for the server, then every
> concurrent execution of psync's schedule, in whatever interleaving each level takes, ends with
> the same client state, the same **server state**, and the same **response for every call** as
> the sequential program.

The proof reuses the whole scheduling argument. `Schedule.lean`'s theorems are restated for any
state and any independence relation under which steps commute (the `Kernel` section). The only new
step is that two independent statements commute in the world (`world_commute`). Their client
updates commute because of the footprints, and their requests commute because the spec is sound.

With the empty specification, in which no two requests commute (`empty_spec_sound`), the theorem
holds for every server. Then no two calls overlap, and only client work and requests to different
servers can. That is the conservative end. [`strict`](./effects.md#unclassified-calls-and-strict)
moves psync towards it for the calls it cannot classify.

## psync's built-in rule is a specification, and it can be wrong

By default, psync decides from names or, with compiler facts, from bodies. A call is a read, a
write or an ordered write of a service, with a target when one is known. Two calls overlap unless
one writes, both are on the same service, and their targets may be the same.

That rule *is* a commutativity specification, so the theorem says exactly when it is safe: when
the rule is sound for the servers involved. Its weakest clause is "two reads always commute".
`World.lean` proves both sides on a small server that counts every request, as a rate limiter
does:

```lean
inductive Req
  | get (k : Nat)   -- returns the value, and bumps the counter
  | count           -- returns the counter, and bumps it

theorem psync_rule_sound_for_gets (k k' : Nat) : Commutes kv (.get k) (.get k')
theorem psync_rule_unsound_for_counters :
    ¬ Sound kv (fun a b => psyncSpec (kvEff a) (kvEff b))
```

Two `get`s commute: the counter ends the same either way, and neither response mentions it. A
`get` and a `count` do not: `count`'s answer depends on whether the `get` came first. psync's rule
calls both reads (`get_value`, `fetch_count`) and would overlap them, so for this server the rule
is unsound, and the guarantee does not apply.

When your server is like this, say so. Make the call that observes the counter a write
(`effects(write(fetch_count))`) or keep it in place (`#[psync::seq]`). Either way, the
specification psync schedules with becomes sound again.

## Specifications for HTTP, S3 and Kubernetes

`verify/Specs.lean` writes three real protocols as server models and proves a specification sound
for each. The models are the assumptions: one copy of the state, requests applied one at a time
(linearizable), and nothing else changing the state behind the client's back.

| Server | The spec allows | Proved | And proved *not* to commute |
|---|---|---|---|
| HTTP (RFC 9110): resources on paths; `GET`, `HEAD`, list are safe; `PUT`, `DELETE` change their target; `POST` does whatever the server does | two safe requests; or two non-`POST` requests on paths **apart** (neither a prefix of the other) | `httpSpec_sound`, for every meaning of `POST` | two identical `PUT`s (`http_put_twice_not_commute`) |
| S3, without versioning | the same; and the same `PUT` or `DELETE` twice | `s3Spec_sound` | two `PUT`s of different bodies to one key |
| Kubernetes API | for clients that ignore `resourceVersion`: two reads; writes to different objects; a list and a write in another namespace | `kube_commutes_up_to_rv` | two writes to *different* objects, exactly (`kube_writes_not_commute`) |

Two of these results correct intuitions that psync's rule relies on:

- **Idempotent is not commutative.** RFC 9110 calls `PUT` idempotent: doing it twice leaves the
  state as doing it once. But the first `PUT` to a new path answers `201 Created` and the second
  `200 OK`, so the two do not commute, and a client that looks at the status sees the order. S3's
  `PutObject` answers with the ETag, which depends only on the body, so there two identical
  `PUT`s do commute.
- **Kubernetes writes to different objects do not commute.** Every write takes the next value of
  one cluster-wide counter, the `resourceVersion`, and returns it. Run two `apply`s in the other
  order and each object gets the other's version. psync's built-in rule, which lets writes with
  different targets overlap, is therefore sound only for clients that do not compare
  resourceVersions across objects. The proof makes this precise. Erasing resourceVersions is a
  simulation: it commutes with every request (`kube_simulation`). On the erased server the spec
  is sound, so requests the spec allows commute *up to* resourceVersions. A client that uses
  `resourceVersion` for optimistic concurrency on one object is unaffected. A client that orders
  events from different objects by `resourceVersion` sees the difference.

## Testing the specification

The specification is the only assumption, and it is about something you can observe. A
server-side log (the Kubernetes API server's audit log, S3 access logs) records which requests
were applied in which order. If two requests that psync ran concurrently were applied in an order
that changed the outcome, that log entry is a **counterexample to the specification**, not just a
failed test. The benchmark harnesses already record these logs (see
[the case studies](../case-studies/kube-datafusion.md)). The specifications above are now
written and proved; checking the logs against them is the next step on the
[roadmap](./roadmap.md).

## What is not modelled

- **One client, one deterministic server.** With other clients, even the sequential program's
  result depends on their timing. The theorem compares the two runs against the same server
  behaviour, and a specification meant to hold under concurrent clients has to say so.
- **The generated Rust**, which rustc type- and borrow-checks.
- **Footprint soundness.** That MIR summaries over-approximate what a function touches is checked
  by `cargo psync check` but not yet proved. That is also on the roadmap.
