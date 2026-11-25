# Certified schedules

The theorems of [Proofs](../foundations/proofs.md) and [the outside world](./world.md) are about
*every* schedule that respects independence. They say nothing about whether the macro, on your
function, produced such a schedule. That is a property of about 5,000 lines of Rust (`psync-graph` and `psync-macros`). psync does not
prove the macro correct. It checks each of its outputs instead.

For every region it makes concurrent, psync can write a **certificate**: what each statement may
read and write, and the levels of chains it chose. A small checker in Lean decides whether the
schedule is safe, and the checker itself is proven sound. This is *translation validation*, the
idea CompCert uses for some of its optimizations: rather than trusting the transformer, check each
transformation's result with something small enough to prove.

## What a certificate says

Every reason psync can give for an edge becomes a **place**, a path of names. Two statements
conflict when one writes a place that the other reads or writes, and places overlap when one is a
prefix of the other:

| Edge reason (`depends`) | Places |
|---|---|
| ``reads `x` ``, ``rebinds `x` ``, ``shadows `x` `` | `n/x`: written where `x` is bound, read where it is mentioned |
| ``writes `self.groups` `` | `p/self/groups`, which overlaps `p/self` |
| an effect conflict | `w/<domain>/t/<target>`: `w/fs/t/` then the characters of `cache.json/`; `w/svc/api/t/…` for a service. An unknown target stops at the domain, and a process or unknown callee at `w` |
| ``may exit before `op` runs`` | `x/i`: written by statement `i` if it may exit, read by later writes |
| `effect order` | `e`: written by every ordinary effect statement |

Targets are spelled out character by character. That makes a `format!("m.key.{id}")` prefix
overlap exactly the strings that start with `m.key.`, as psync's own rule says. Where psync's rule
is finer than prefixes (services named by type, ordered writes), the place is cut short, so the
certificate only ever has *more* conflicts than psync. The property test
`certificates_cover_every_edge` checks that over every annotated function in the tests, the driver
fixture and `graph_suite`: each of psync's edges is a conflict between places.

For `profile_page`, with names interned to numbers:

```lean
def profilePage : Cert where
  nodes := [
    { reads := [[20], [30]], writes := [[10]] },         -- user = api.user(..)
    { reads := [[20], [30]], writes := [[11]] },         -- posts
    { reads := [[20], [10], [30]], writes := [[12]] },   -- avatar reads user
    { reads := [[20], [30]], writes := [[13]] } ]        -- friends
  levels := [[[0, 2], [1], [3]]]

theorem profilePage_ok : check profilePage = true := by decide
```

## The checker, and why it is sound

`check` (in `verify/Cert.lean`) asks two questions:

1. Are the chains of each level pairwise independent?
2. Is running the levels' chains one after another a *reordering of the program past independent
   statements only*? It walks the program in order. Each statement must find, scheduled ahead of
   it, only statements independent of it.

`check_sound` proves what a yes means:

> If `check c = true`, then for any meaning of the statements under which the certificate's
> independence is real independence, **every** run of the schedule, in any interleaving of each
> level's chains, ends as the program run in order does.

"Any meaning" includes `World.lean`'s client and server world. So with a sound commutativity
specification, a certified schedule preserves the server's state and every response too.

Lean checks each certificate by `decide`: it runs `check` inside the proof checker and accepts
only if the answer is `true`. A wrong schedule does not compile:

```text
error: Certs.lean:31:38: tactic 'decide' proved that the proposition
  check c1 = true
is false
```

## Running it

```sh
cargo run -p psync-graph --features cli --bin psync-cert -- src/lib.rs > verify/Certs.lean
cd verify && lake build GeneratedCerts
```

`scripts/certify.sh` does this for the repository's own schedules: 29 regions from the graph tests,
the driver fixture and `graph_suite`, checked in a few seconds. CI regenerates the certificates,
fails if they changed without being committed, and checks them with Lean.

Building the certificates found one false alarm, and no unsafe schedule. The first version of the
place encoding dropped `format!` targets, and Lean rejected `graph_suite`'s matrix-rust-sdk
`disable` because of it: psync had correctly overlapped a write to `m.secret_storage.key.{id}` with
writes to `m.megolm_backup.v1` and five others. The character encoding fixed it.

## What a certificate does not cover

- **That the places describe the statements.** This is the hypothesis of `check_sound`. psync
  computes the places from tokens, and from compiler facts under `cargo psync build`.
  [`cargo psync check`](./check.md) checks them against MIR, but that is not yet proved: proving it
  is the footprint-soundness work on the [roadmap](./roadmap.md).
- **Prefetched loops.** Certificates cover regions. A loop's prefix is checked by the macro's
  refusal rules, and certifying it is the next step.
- **Exit replay.** `replay_correct` in `Schedule.lean` proves the rule in general. Certificates do
  not yet record each statement's exits.
- **The generated Rust**, which rustc type- and borrow-checks.
