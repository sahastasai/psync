# Brainstorm, 2026-11-24: novelty, Phase II, and performance diagnosis

Owner's direction. psync should become a tool that:
- extracts parallelism in a novel way, guaranteeing safety with something non-trivial;
- diagnoses performance.

It should answer the professor's concern: "nothing can be guaranteed about concurrent accesses
from MIR, so this is not novel."

## 0. What the conversation was about

The professor described a former student's project. The student modified a Swift parser to
analyze concurrency in an iOS app, and graduated before finishing. The professor proposed a
graph **richer than a dependence graph**, one that captures:
- conditions (`if` branches);
- the order between async calls;
- branches that spawn new tasks or threads;
- merge points.

Starting from an entry point, such as a button click, the graph would carry measured latencies.
The question it answers: *under which conditions* is which path critical, up to the last screen
update the user sees? That gives developers insight, or drives an automatic fix. The open question
was whether this fits Rust, or mobile apps, where UI threads and spawned threads are the norm.

Two worries were raised:
- **Calls might depend through the server.** Two async calls may depend on each other through
  state the analysis cannot see. If so, assume they depend, and run them in order.
- **The goal is not only parallelism.** It is to find the bottleneck, then target it.

## 1. The concern, stated precisely

- **Accepted.** No client-side IR can see a server's state. A `GET` may bump a rate-limit
  counter, refresh a session, or fill a cache. Two "reads" may therefore not commute at the
  server, and MIR cannot tell.
- **Rejected.** That does not mean nothing can be guaranteed. Every correct reordering tool
  guarantees equivalence *relative to a model of the outside world*. ReSynchronizer's model is
  two global pseudo-variables. DBridge's model is "the database is one variable". Neither makes
  its model explicit, checks it, or proves the reordering against it.

psync can do all three, and that is the novelty:

| Layer | What psync knows | Where from | Strength |
|---|---|---|---|
| Client memory | no two concurrent branches race, and borrows end before moves | Rust's ownership types, checked by rustc on the generated code | **sound** (rustc) |
| Client I/O footprint | exactly which files, sockets, processes and calls each branch reaches, through every callee and every `dyn` implementation | MIR summaries plus link-time class-hierarchy analysis | sound up to stated gaps (FFI, function pointers) |
| External world | which operations commute | an explicit **commutativity specification**: HTTP RFC 9110, API conventions, or user-declared | **assumed**, made explicit, and **testable** |
| Composition | schedule equivalent to the sequential program | Lean reduction theorem | **machine-checked** |

The claim to prove: *for every program rustc accepts, the external trace of psync's schedule is
equivalent to the sequential program's trace, modulo the commutativity specification* (Mazurkiewicz
trace equivalence). The server's state after the run, and every response the client sees, are then
the same as sequentially. Without a specification, psync falls back to "all operations on one
service conflict" and is still sound. It then overlaps only across services, files and
client-local work.

No dynamic language can build the first row soundly. That is why the JavaScript tools call
themselves unsound.

### 1a. Formal verification plan (bundle "Formal verification")

1. **The world as a transition system.**
   - **Model:** extend `verify/Schedule.lean` so that operations act on *(client state, server
     state)*. The server is an abstract labelled transition system. A specification gives a
     commutativity relation on server operations, and the relation may be state-dependent.
   - **Theorem:** psync's schedule preserves the server's final state and every response,
     whenever reordered operations commute under the specification.
   - **Reads that write:** model them directly. A `GET` that increments a counter commutes with
     another increment, not with a read of that counter.
2. **Certified schedules (translation validation).**
   - The driver emits a certificate for every rewritten function: the nodes with their MIR-derived
     footprints, the edges, the chains, the levels, and the loop prefixes.
   - A Lean checker (`check : Cert → Bool`) is proven sound: `check c = true → schedule ≡ program`.
   - CI checks every build's certificates with `decide`. Each build then carries a machine-checked
     proof, not just the general theorem. This is CompCert's translation-validation idea, applied
     to concurrency extraction.
3. **Footprint soundness.**
   - Formalize a small MIR fragment: places, calls and aggregates.
   - Prove that the summary rules over-approximate what a function can touch, which discharges the
     theorem's assumptions.
   - Charon/Aeneas (MIR → LLBC → Lean) is worth a feasibility test on the non-async parts.
4. **Specifications that can be tested.**
   - Encode commutativity specifications for HTTP (safe and idempotent methods), the Kubernetes
     API and S3.
   - Conformance-check them against server-side logs (the kube-apiserver audit log, S3 access
     logs) in the existing harnesses. A schedule that changed the server's history is a
     counterexample to the specification, not just a test failure.
5. **Exit replay** in Lean (already open).

## 2. Phase II: synchronous code

Two kinds of opportunity exist:
- **Blocking I/O in sequence:** `std::fs`, `reqwest::blocking`, synchronous database drivers,
  and `Command::output` in CLIs and build tools.
- **Loops of independent CPU work.**

Use the same graph, effects and proofs. Execute through `std::thread::scope`, which borrows
safely, or `MultiExecutor`, with a cost model so that small work stays sequential.

Steps:
1. **Design:** region extraction in synchronous functions, `Send`/`Sync` obligations, and a
   granularity and cost model.
2. **Prototype:** `#[psync::graph]` on a `fn`. Overlap independent blocking calls with scoped
   threads, and parallelize carried-state-free loops.
3. **Audit:** reuse the 35-repository audit tooling to find blocking sequences and parallel loops
   in real projects. Measure them, and report where Phase II helps and where it does not
   (thread overhead, contention).

## 3. Diagnosis: a graph richer than a dependence graph

The professor's graph is a **program dependence graph** (Ferrante, Ottenstein and Warren, 1987),
extended for async code. Call it an *async PDG*. It is built from MIR:
- **Control dependence:** from post-dominators, with each node guarded by the branch conditions
  (`SwitchInt`) that decide whether it runs.
- **Data and effect dependence:** psync's edges.
- **Fork and join:** `spawn`, `join!`, `select!`, and psync's own levels.
- **Loops:** summarized by their trip counts.

Each node is annotated with measured latency distributions from `tracing` spans or tokio's task
instrumentation, joined to MIR locations.

Analyses on top:
- **Conditional critical path.**
  - For each feasible combination of guards, find the longest path from the entry, for example a
    request handler or a UI event, to the exit or the last render.
  - Report it as: *"when the cache misses, the critical path is A → B → C (120 ms); B and D are
    independent, so psync would make it 70 ms."*
  - Enumerating the combinations of path conditions is exponential, so it needs bounding, by
    profile frequency or by symbolic merging.
- **What-if.** Predict the latency after psync's rewrite, or after a hint, before applying it,
  then verify by measurement. This is like Coz's causal profiling (SOSP 2015), but for a concrete,
  verified transformation.
- **Fix.** Apply the rewrite with a certificate (section 1a.2), or tell the developer which
  dependency to break.

Which entry points fit Rust:
- **Server request handlers** (axum, actix): latency per request.
- **CLIs:** the user waits for exit.
- **UI event handlers.** Dioxus, Tauri, egui and Zed's GPUI are Rust UIs, so click-to-last-render
  fits Rust as well as mobile does. This is also where the professor's student project lived.

Related work to position against:
- AppInsight (Ravindranath et al., OSDI 2012): critical paths of user transactions in mobile apps,
  the closest to the student's work;
- Panappticon, Timecard;
- Coz;
- the PDG literature, and the parallelizing compilers built on it.

**psync's angle.** Path conditions come from the IR. The fix is automatic, and both the fix and
the predicted path are checked against the same verified schedule semantics.

## 4. How the pieces make one story

> **psync builds an async PDG from MIR.** It annotates the graph with sound client-side
> footprints (Rust ownership plus whole-program effect summaries) and with measured latencies.
> It uses the graph to *diagnose*: conditional critical paths. It uses the graph to *fix*:
> parallel schedules for async code (Phase I) and synchronous code (Phase II), each with a
> machine-checked certificate of equivalence relative to an explicit, testable model of the outside
> world.

## 5. Risks

- **Specifications may be wrong.** Mitigation: the conformance checks in 1a.4, and the
  conservative default.
- **Certificate size.** Large functions may make checking slow. Mitigation: chains are small in
  practice, and checking is linear-ish in the number of edges.
- **Aeneas does not support async.** Plan around it: certify the schedule, not the code.
- **Rust vs mobile.** Rust UIs are a smaller community than iOS and Android. The diagnosis applies
  to Rust servers and CLIs first, and to UIs as a showcase (Zed, Dioxus).
