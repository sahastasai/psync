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
| hidden state, from the compiler's facts | `h/<crate>/<path>` for a `static` or `thread_local!` (`h/graph/PEAK`); `n/x/$state` for a cell, lock or atomic behind the reference `x`, plus a read of `h/$ref`; `h` alone for state behind a reference the driver could not follow (it overlaps every `h/…`) |

The last row comes from the compiler, not from the tokens: see
[Hidden state](#hidden-state-statics-thread-locals-and-cells) below.

Targets are spelled out character by character. That makes a `format!("m.key.{id}")` prefix
overlap exactly the strings that start with `m.key.`, as psync's own rule says. A placeholder in a
`format!` target is a **hole** segment (the certificate lists which segments are holes). Two
places overlap if they agree until one ends, or if they first differ where either has a hole,
since a hole may stand for any text. Without holes, that is exactly "one is a prefix of the
other". `format!("{dir}/a")` and `format!("{dir}/b")` agree up to `{dir}/` and then differ in
plain characters, so they do not overlap. Where psync's rule
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
cargo psync check                                       # writes target/psync/facts.tsv
cargo run -p psync-graph --features cli --bin psync-cert -- \
    --facts target/psync/facts.tsv src/lib.rs > verify/Certs.lean
cd verify && lake build GeneratedCerts
```

Without `--facts`, the places come from the tokens alone, and a certificate omits whatever the
callees do out of sight. With it, every place the compiler found is added (next section).

`scripts/certify.sh` does this for the repository's own schedules: 47 certificates from the graph
tests, the Phase II tests, the driver fixture and `graph_suite`, checked in a few seconds. Their
facts are committed as `verify/facts.tsv`; `scripts/cert-facts.sh` regenerates them with
`cargo psync check` (it needs psync-check's pinned nightly). The checker is the same for
synchronous code: a level is a set of chains, whether they run as futures or on threads. CI
regenerates the facts and the certificates, fails if either changed without being committed, and
checks the certificates with Lean.

## Hidden state: statics, thread-locals and cells

A proc macro cannot see callee bodies, so the places above miss what a callee does to memory
that no argument names. `io` in `tests/graph.rs` bumps thread-local counters, `slow` in
`tests/sync.rs` pushes onto a `Mutex` behind a `&`, and `graph_suite`'s stubs push onto a global
timer queue. `psync-driver` reads the compiled bodies (MIR), so it is where this information comes
from. For every function of the workspace it records, next to file and service effects:

- **a `static` or `thread_local!`** it names, directly or through any callee: written, unless
  the method only reads (`Cell::get`, `load`, `RefCell::borrow`, `RwLock::read`, a `thread_local!`
  `get`). A `static` whose type has no interior mutability cannot change and is left out; a
  `static mut` is always written. A lazy value (`LazyLock`, `LazyCell`) counts as read when
  what it holds cannot change behind `&`, and as written otherwise;
- **a cell, lock or atomic behind a reference** (`Cell`, `RefCell`, `Mutex`, `RwLock`, atomics,
  `OnceCell`/`OnceLock`, `Condvar`, anything whose type is not `Freeze`) that a standard-library
  call changes through `&`. The driver follows the reference back to where it came from: a
  parameter (then the caller substitutes what it passes, as for file paths), a static, or a value
  the function owns (no other statement can reach it, so nothing is recorded). A pointer read from
  inside an object is named by that object: what the `&'static Timers` inside `TIMERS` points to
  is `TIMERS`. When the origin is unknown, the effect is on *any* state.

The facts file gives each call site of a `#[psync::graph]` function these effects, and
`psync-cert` turns them into places: `h/<static path>`, `n/x/$state` when the state is behind
the argument `x` (it overlaps every use of `x`), and `h` for any state. It rounds toward conflict:
a call it cannot follow writes `h`, and an argument that is not a plain binding writes `h` too.

The schedule is still the one the macro builds from the tokens, as `cargo build` compiles it.
`psync-cert` expands each function twice, without the facts (the schedule) and with them (the
extra places), and the certificate's nodes are the union. If the schedule is unsafe with those
places, `psync-cert` fails, and `cargo psync check` reports the pair as `shared-state` (a warning,
an error with `--deny`). `cargo psync build` and `fix` then order the two statements.

### Observed state

Some hidden state is written on purpose to *measure* the schedule. The graph tests count how
many requests are in flight (`PEAK` is 1 in program order and up to 4 concurrently, and the tests
assert both), the Phase II tests log when each call ran, and `graph_suite`'s stubs sleep on a
shared timer. A concurrent schedule changes that state: that is what the tests check. Such state is
marked, and stays in the certificate:

```rust,ignore
thread_local! {
    #[psync::observed]                       // a static or thread-local
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

#[psync::graph(observe(log))]                // the state behind a binding of the function
fn three_downloads(log: &Mutex<Vec<&'static str>>) -> (u32, u32, u32) { .. }
```

For such a certificate `psync-cert` writes two theorems:

```lean
def c14_observed : List Place := [[0, 33, 36]]            -- n/log/$state
theorem c14_ok : check (c14.hiding c14_observed) = true := by decide
theorem c14_strict : check c14 = false := by decide
```

`Cert.hiding` removes the observed places from the nodes. By `check_sound`, the first theorem
says that every run of the schedule ends as the program run in order does *on everything but
the observed state*. The second says, also by `decide`, that counting it the schedule is not
equivalent. That is the honest claim for a test that measures concurrency. Observed state orders
nothing in the macro and is never reported by `cargo psync check`.

### What this changed

All 41 earlier certificates, plus 6 new ones in the driver fixture (below), were regenerated with
the facts. No schedule changed. With the facts:

| | Certificates | Why |
|---|---|---|
| safe as they are | 7: c17, c21, c24, c28, c30, c32, c37 | nothing hidden meets |
| safe but for observed state | 27: c0–c16, c18, c38–c46 | the graph tests' counters (`ACTIVE`, `PEAK`, `TICKS`), the Phase II tests' `log`, the server log of `Store` in `reads_then_one_write`, `graph_suite`'s timer (`TIMERS`, `SEQ`), the fixture's `PROBES` |
| unsafe, and proved so (`check cN = false`) | 13: c19, c20, c22, c23, c25–c27, c29, c31, c33–c36 | the fixture's planted effects: the files that `start`, `login`, `snapshot`, `tour`, `release`, `stash_same` and `keep_same` write, `boot`'s panic, and the hidden state of `shared`, `counted`, `journaled`, `nested` and `appended` |

So every certificate now lists the places that the driver found its statements touch, and Lean
decides each one: 34 hold outright or but for state marked observed, and the 13 planted ones are
refuted. (#25 expected c18 to hold as it was; it does not, because its calls of `slow` push onto
the observed `log`.) The fixture is the one file whose unsafe schedules are expected:
`psync-cert --expect-unsafe psync-check/tests/fixtures/` writes `theorem cN_rejected : check cN
= false` there, and fails on an unsafe schedule anywhere else.

The new fixture functions are the regression tests: a static atomic (`counted`), a static
`Mutex` (`journaled`), a `thread_local!` (`nested`), a `Mutex` behind a reference (`appended`),
each touched by two statements that psync runs on two threads. Each is refused three ways:
`cargo psync check` reports `shared-state`, `cargo psync build` orders the two statements, and
Lean refutes the certificate. Two different statics (`hit_and_miss`, c37) stay concurrent and are
proved; two calls that bump an observed counter (`probed`, c38) are proved but for it.

### What the facts do not see

The places are those the driver can see, so the claim is "every place the driver found", not
"every place". Still assumed:

- **Published crates' own state.** Statics of crates from crates.io or git (a runtime's context,
  a logger, a cache) are left out, as their sockets are: the driver assumes they do not change
  what the function computes. psync's own runtime is left out the same way.
- **What the driver cannot follow.** Function pointers, `dyn` methods with no implementation in
  the build, FFI, drop glue and inline assembly. Raw pointers that the driver cannot trace are
  "any state" only when they reach a standard-library call on a non-`Freeze` type; writes
  through a raw pointer by hand are not seen.
- **Aliasing between references.** The state behind `x` is named by the binding `x`. Two
  different bindings are taken to reach different state, unless one is computed from the other in
  the region (then data flow orders them). So two parameters that the caller points at the same
  cell, or a reference parameter that points to a static another statement names directly, are a
  conflict the certificate misses. Only a reference the driver could not follow is "any state"
  (`h`, which every `h/…` and `h/$ref` meets).
- **Generic code.** A summary is computed once for a generic function. State behind a reference
  to a type parameter counts only if the type is a cell, lock or atomic for every instance.
- **The facts are as current as the last `cargo psync check`.** CI regenerates them; locally,
  `scripts/cert-facts.sh` does. A stale fact (the callee's name no longer matches its site) is
  ignored rather than misapplied, which loses places, so CI's diff check is what keeps them.

Building the certificates found one false alarm, and no unsafe schedule. The first version of the
place encoding dropped `format!` targets, and Lean rejected `graph_suite`'s matrix-rust-sdk
`disable` because of it: psync had correctly overlapped a write to `m.secret_storage.key.{id}` with
writes to `m.megolm_backup.v1` and five others. The character encoding fixed it.

## Footprints, proved for a fragment of MIR

`check_sound` assumes that a certificate's places are right: if the certificate says statement 3
writes only `s.a`, then it really does. `verify/Footprint.lean` proves that assumption for a small
language shaped like MIR, the compiler's simplest form of a function
([From MIR to summaries across crates](../internals/compiler.md) explains how psync reads MIR):

- **Places** are a local and a path of fields: `_5.0.1` is field 1 of field 0 of local 5.
- **Statements** assign the result of an operation on places and constants (`_1 = _1 + 1`),
  move a whole struct (`_2 = move _3`), and combine through sequencing, `if`, `while` and calls.
  A call passes places by reference, as `drain(&mut s.a, &mut s.b)` does.
- **A summary** of a function lists the places it may read and write, in terms of its own
  parameters. A caller never looks at the callee's body. It takes the summary and replaces each
  parameter with the argument: `_0` becomes `s.a`. This is how psync's summaries cross crates.
- **The link check** (`Valid`) asks, for every function, whether its summary covers what its body
  does, where the body's calls are again read from summaries. It is one local check per function,
  so it works for recursion and for functions from other crates, and for a finite program it is
  decided by computation (`Program.validB`).

Two theorems follow, for every function and every input, provided the link check passes:

- **Frame.** A statement changes nothing outside the places it may write.
- **Dependence.** What it writes depends only on the places it may read or write. (Writes count
  as reads, because a statement that only sometimes writes `x` leaves the old `x` otherwise.)

Together they make statements with disjoint footprints commute, and `hind_of_cover` turns a
certificate that covers the footprints into exactly the `hind` that `check_sound` needs. The
result, `mir_schedule_sound`, has no assumption left about footprints: if the link check passes,
the certificate covers the summaries, and `check` accepts, then every run of the schedule ends with
the same memory as the sequential program. The file's example is a recursive `drain` called on two
fields of one struct, in one level. Its summary is checked against its own body, and a summary that
forgets one write is rejected.

### From real Rust, through Charon

The fragment is small, but real compiler output fits it. [Charon](https://github.com/AeneasVerif/charon)
extracts a crate's MIR, after borrow checking, as a structured program (LLBC).
`scripts/charon/llbc2lean.py` translates that into the fragment:

- `*x = *x - 1` on a `&mut u64` parameter becomes an assignment to the callee's place `x`;
- `_4 = &mut (*s).a`, and the reborrow `_3 = &mut *_4`, are resolved to the place they borrow,
  `s.a`, as the borrow checker names it;
- a call `drain(move _3, move _5)` becomes `call drain σ`, with `σ` sending `drain`'s
  parameters to `s.a` and `s.b`, and its own locals to a frame region of the caller, one per call
  site;
- an `if` whose branch returns early takes the rest of the block into the other branch.

On the crate in `scripts/charon/drain`, the Rust version of the example above, the script writes
`verify/CharonDrain.lean`. Lean then decides, without trusting the script, that the summaries pass
the link check, that a certificate running both calls of `callers` in one level covers their
footprints, and that `check` accepts it. `mir_schedule_sound` then gives the theorem for the
compiled code. Two red checks show the pipeline can fail. If the second call is changed to
`drain(&mut s.b, &mut s.d)`, Lean rejects the certificate. If `drain`'s summary forgets its write
to `*y`, Lean rejects the link check.

**Aeneas**, built on Charon, goes further: it turns the same LLBC into pure Lean functions, with
each `&mut` replaced by a value passed in and returned (`scripts/charon/aeneas/Drain.lean.txt`):

```lean
def callers (s : S) : Result S := do
  let (i, i1) ← drain s.a s.b
  let (i2, i3) ← drain s.c s.d
  ok { a := i, b := i1, c := i2, d := i3 }
```

There, "the two calls commute" is an ordinary equation, `callers s = callers_swapped s`. It even
shows why psync replays exits in program order: in `Result`, if both calls failed, the two orders
would return different errors. The cost is a second proof world. Aeneas's library needs Lean 4.36
and Mathlib, while `verify/` uses Lean 4.22 and only Lean's core library, and every function needs
its own functional proof. For psync, whose question is only *which places* a call touches,
the footprint route (Charon into the fragment, then `decide`) is the one that scales. Aeneas is
the tool for the rare call whose commutation depends on *values*.

The translation itself is not proved. It is a small script, it stops with an error on anything
outside the fragment (loops, `dyn`, by-value arguments, unwinding), and its output is checked, not
trusted, for everything except its faithfulness to the LLBC.

What the fragment leaves out, and so stays assumed for real Rust:

- **References stored in memory** (`*p` where `p` was computed). The borrow checker is what lets
  psync name the place behind a `&mut`, and it is not modelled.
- **Interior mutability and `unsafe`**, which let a shared reference write.
- **Panics and unwinding.**
- **That real MIR is faithfully translated into the fragment**, and that psync's emitter writes
  certificate places that cover the summaries it computed.

## The generated certificates against compiled code

`CharonDrain.lean` is one crate written for the purpose. The 47 certificates in `Certs.lean` are
psync's real output for the tests, the driver fixture and `graph_suite`. Nothing yet ties any of
them to compiled code. This section says why, and how it will be done.

### Deciding `hind`, not comparing names

A certificate names source bindings and paths (`n/a`, `p/self/groups`, `w/fs/t/…`). Compiled code
names MIR locals and fields. Translating one naming into the other would be a second trusted
script. `Footprint.lean` now avoids it: `hindB` takes the certificate as `psync-cert` wrote it and
a list of compiled statements, one per node, and decides that every pair of nodes the certificate
calls independent has disjoint real footprints. That is exactly `check_sound`'s hypothesis `hind`
(`hind_of_hindB`), so `cert_schedule_sound` needs no assumption about footprints: link check,
`hindB` and `check` by `decide`, and every run of the schedule equals the statements in order.
`callers_hindB` checks it on `drain`. `callers_shared_hindB` is the red check: a second call that
shares `s.b` with the first is rejected, though the certificate still calls the nodes independent.

So the claim to prove for each certificate is "its independence is real independence". That is
weaker than "its places equal the real places", and it is what soundness needs.

### What each certificate's statements do

Every node mixes three kinds of places:

- **client places**, `n/x` and `p/a/b`. They are the statement's own bindings and paths. This is
  `Footprint.lean`'s job.
- **world places**, `w/…`. They come from callees: files, services, processes. They belong to
  `World.lean` and its commutativity specification, or, for files, to a model of the file system
  as memory indexed by path.
- **`x/i` and `e`.** They are psync's own encoding of exit order and effect order. They are justified by
  `replay_correct` and `recording_slot`, not by any callee's footprint.

| Certificates | Functions | Can the current pipeline extract them? | Hidden state the certificate now lists |
|---|---|---|---|
| c0–c13 | `tests/graph.rs`: `graph_tree`, `chains`, `first_error_wins`, `exit_after_later_level`, `options`, `shadowing_and_mut`, `early_return`, `awaited_arguments`, `effects_keep_order`, `fresh_pool`, `Aggregated::run`, `reads_then_one_write`, `await_inside_a_binding`, `crossing` | No: `async fn` (coroutines). `io` also uses `poll_fn` closures, loops and generics | `io`'s thread-local counters `h/graph/ACTIVE`, `PEAK`, `TICKS` (observed); `logged`'s `RefCell` behind `log` (c3); the `Store` log behind `store` (c11, observed) |
| c14–c16, c18 | `tests/sync.rs`: `three_downloads`, `first_error_wins`, `panic_after_error`, `speculative_writes` | No: `thread::sleep` and `Mutex::lock` have no body, generics, `?` and `expect` (unwinding) | `slow`'s `Mutex` log behind `log`, `n/log/$state` (observed) |
| c17 | `returns_future` | No: async blocks | nothing |
| c19–c27, c29, c31 | fixture: `start`, `login`, `sizes`, `shared`, `snapshot`, `walk_then_read`, `tour`, `release`, `boot`, `stash_same`, `keep_same` | No: `async fn`, plus `dyn` (c23), mutual recursion through `Box::pin` (c24, c25), and other crates' functions (c21) | the planted files and the panic, and `shared`'s `RefCell`: refuted, except `sizes` and `walk_then_read` |
| c28, c30, c32 | fixture: `stash_both`, `keep_both`, `stash_tail` | Not yet for c28 and c32: synchronous, and the blockers are small (`fs::write` has no body, `impl AsRef<Path>` is a by-value generic parameter, `r1?` is an exit). c30: `async fn` | each call's own file, `w/fs/t/a.json/` and so on: they never meet |
| c33–c38 | fixture: `counted`, `journaled`, `nested`, `appended`, `hit_and_miss`, `probed` | Not yet: atomics, `Mutex` and `thread_local!` have no body | `h/app/HITS`, `h/app/JOURNAL`, `h/app/DEPTH`, `n/log/$state`, `h/app/MISSES`, `h/app/PROBES` (observed): c33–c36 refuted |
| c39–c46 | `graph_suite`: `Discovery::run`, `put_and_upload_sst`, `checkpoint_sync`, `fetch_extensions_from_blob_store`, `process_show_validators`, `disable`, `fetch_page`, `check_notifications_allowed` | No: `async fn`. c39 and c42 nodes are also the macro's own `join_all_bounded` code | the suite's timer, `h/graph_suite/runtime/TIMERS` and `SEQ` (observed). The world places are more than the stubs touch |

None of the 47 fits the current fragment, so none is tied to compiled code yet. What changed is
the other half of `hind`: before, 34 of the 41 certificates omitted real writes, so `hind` was
false for them in the strict sense, and nothing said so. Now every certificate lists the state
the driver found, and Lean decides each one with it (see [What this changed](#what-this-changed)):
the planted fixture cases are refuted, and the tests that measure concurrency are proved
equivalent on everything but the observed state. What remains for each is extraction: showing,
from compiled code, that the places are complete.

### The route

1. **The three synchronous fixture functions** (c28, c29, c32). Compile the fixture as written,
   with `#[psync::graph]` replaced by a macro that only removes psync's statement attributes, so
   that Charon sees the program in order. Split the function's LLBC into nodes by source line:
   `psync-cert` needs to write each node's first and last line. Model a constant path as a reference to a file place
   (`fs::write(p, d)` writes `*p`) and `AsRef::as_ref` as the identity on the reference. Model
   `r1?` as recording its outcome in its own slot (`recording_slot`). Lean then decides
   `hindB` for `PsyncCerts.c28` and `c32` (expected: true, so `cert_schedule_sound` applies), and
   refutes it for `c29` (both calls write `a.json`). Since the facts, `check` already refutes
   c29 by itself: its certificate names `a.json` in both nodes.
2. **Interior mutability as places.** Done on the certificate side
   ([Hidden state](#hidden-state-statics-thread-locals-and-cells)): statics, thread-locals and
   cells behind references are places, c22 (`shared`) and c33–c36 are refuted, and the tests that
   measure concurrency are proved with their observed state hidden (`Cert.hiding`). Extracting
   it needs models of `Cell::set`, `Mutex::lock` and `LocalKey::with` as writes to such places.
3. **Coroutines.** The Phase I functions need an extractor that handles `async fn`. Either
   Charon's support for coroutines, or `psync-driver`, which already reads the coroutines'
   MIR for `cargo psync check`, exporting bodies in the fragment. A statement `let a = f().await`
   is then the call of `f`'s coroutine body to completion. Its footprint is `f`'s summary, and the
   link check stays as it is.

What stays assumed on every step: that the extraction is faithful to the compiled code, the
models of functions without a body (`fs::write`, `Mutex::lock`), and the gaps of the fragment
(references stored in memory, `unsafe`, unwinding).

## What a certificate does not cover

- **That the places describe the statements.** This is the hypothesis `hind` of `check_sound`.
  For a fragment of MIR it is proved: `verify/Footprint.lean` (above). For the 47 generated
  certificates it is not yet proved for any of them (see the table above). Their places come from
  the tokens and from the driver's facts about the compiled callees, including hidden state, so
  what they still omit is what the driver cannot see
  ([What the facts do not see](#what-the-facts-do-not-see)). The gap between real MIR and the
  fragment is what stays assumed.
- **Observed state.** Where a certificate hides observed places, its theorem is about everything
  else. That the hidden state is only measured, and never feeds a result, is the programmer's
  claim when they write `#[psync::observed]` or `observe(..)`.
- **Prefetched loops.** Certificates cover regions. A loop's prefix is checked by the macro's
  refusal rules, and certifying it is the next step.
- **Exit replay.** `replay_correct` in `Schedule.lean` proves the rule in general. Certificates do
  not yet record each statement's exits.
- **The generated Rust**, which rustc type- and borrow-checks.

## Exercises

Each answer is what `check` returns. To try them, add the lines to a file that imports `Cert`
and `open PsyncCert`, and run it with `lake env lean` in `verify/`:

```lean
#eval check { profilePage with levels := [[[0, 1], [2], [3]]] }
```

1. `[[[0, 1], [2], [3]]]`: `user` and `posts` as one chain, `avatar` and `friends` beside it.
2. `[[[0], [1, 2], [3]]]`: `avatar` chained after `posts` instead of `user`.
3. `[[[0], [1], [3]], [[2]]]`: `avatar` alone, in a second level.
4. `[[[0, 2, 1, 3]]]`: one chain, with `avatar` moved ahead of `posts`.
5. psync's edge reason is ``writes `self.groups` ``. Why is the place `p/self/groups`, and why
   does it conflict with a statement that reads `self`, but not with one that reads
   `self.mode`?

<details>
<summary>Answers</summary>

1. `false`. `avatar` reads `[10]`, which `user` writes, and the two are in different chains of
   one level.
2. `false`, for the same reason. Chaining it after `posts` does not order it after `user`.
3. `true`. It is slower than psync's schedule, but safe: `avatar` starts after the first level,
   including `user`, has ended.
4. `true`. A single chain runs in its own order, and the second question allows that order:
   `avatar` passes only `posts`, which it is independent of. `check` accepts every safe
   schedule, not only psync's.
5. A place is a path of names, and two places overlap when one is a prefix of the other.
   `p/self` is a prefix of `p/self/groups`: reading all of `self` reads `groups`. `p/self/mode`
   and `p/self/groups` first differ at the third segment, so they are apart.

</details>
