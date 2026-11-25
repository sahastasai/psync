# Roadmap: beyond tokens

The [effect analysis](./effects.md) closed the largest gap: psync no longer runs recognized writes
before an earlier exit, and it keeps operations on one file or service in order. But it still
reads names, not meaning. This chapter collects what would make the guarantees stronger, roughly
in the order it would pay off. Each item says what problem it solves, the idea, and what it costs.

The goal stays the one in the proposal: *the generated program is logically identical to the
sequential one*. In practice that means two properties:

1. **Same exits.** The function returns the same value or error from the same place. psync
   guarantees this today by replaying exits in program order.
2. **Same effects.** The outside world sees the same operations, each resource in the same order,
   and nothing the sequential program would not have done. psync guarantees this today only for
   effects it recognizes, and only as far as targets can be told apart from tokens.

Everything below strengthens the second property.

## 1. Runtime disambiguation of targets

**Problem.** `fs::write(&a, ..)` and `fs::read(&b)` conflict today, because `a` and `b` may name the
same file. In a loop, psync *assumes* that items name different objects, which is false for a list
with duplicates.

**Idea.** Do what vectorizing compilers do with runtime alias checks. When both targets are pure
expressions, evaluate them first and branch:

```rust,ignore
if __psync_target(&a) != __psync_target(&b) {
    /* concurrent schedule */
} else {
    /* sequential schedule */
}
```

For loops, collect the targets of the prefetched writes into a set before starting. Fall back to
sequential order from the first duplicate. File paths can be compared after lexical normalization,
and optionally after `canonicalize` (one extra syscall per path).

**Result.** The loop assumption becomes a check, and computed targets stop blocking concurrency.
**Cost.** Code size, one comparison per pair, and the rule that target expressions must be pure.

## 2. Effect contracts checked by the type system

**Problem.** Names lie. `cache.get(k)` may write a refresh, and `factory.create(..)` may only build a
value in memory. A proc macro cannot look up what a callee does.

**Idea.** Let crates declare effects on *types*, and let rustc check psync's guess. Most client APIs
already give each operation its own type: AWS fluent builders (`GetObjectFluentBuilder`), kube's
`Api<K>` methods, reqwest's `RequestBuilder`. psync would ship a trait:

```rust,ignore
pub trait Effect { const KIND: EffectKind; }       // Read, Write, Ordered
impl Effect for aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder {
    const KIND: EffectKind = EffectKind::Read;
}
```

Those impls would live in small `psync-effects-*` crates per ecosystem, which the orphan rule
allows because psync owns the trait. At every call it classified, the generated code would emit a
zero-cost assertion, `psync::__private::claims::<Read, _>(&builder)`. When the type has an impl
that disagrees with the name, compilation fails with a message pointing at the call. For your own
functions, `#[psync::effect(read)]` on the definition would generate the impl for a marker type
that the function returns.

**Result.** Name-based classification becomes a *checked* claim wherever an ecosystem crate covers
the API, which is most of the I/O in the audit.
**Cost.** Maintaining impls for popular crates, and `const` evaluation in generated code.

## 3. An effect sanitizer for tests

**Problem.** Static analysis will always miss something: a callee that writes, two clients to one
server, a symlink. ReSynchronizer and similar tools rely on the project's tests passing, which
says little about effects that tests do not assert on.

**Idea.** A dynamic checker in the spirit of ThreadSanitizer, but for the outside world:

1. With `PSYNC_AUDIT=1`, generated code wraps each chain's `poll_step` in
   `__private::enter(chain_id)`/`leave()`, which sets a thread-local *current chain* and records
   when each chain started and finished, and whether its result was used or discarded by an
   earlier exit.
2. An effect log captures what actually happened, attributed to the current chain:
   - **files and processes**: an `LD_PRELOAD` shim, or `seccomp` user notification on Linux, records
     `openat` with write flags, `unlink`, `rename`, `mkdir`, `execve`;
   - **network**: a `tracing` layer, or a `tower`/`reqwest` middleware that records method and URL,
     and a hyper header (`x-psync-chain`) for server-side logs;
   - **services**: kube's and the AWS SDK's existing interceptors.
3. After the test, the checker reports
   - **speculative writes**: a write in a chain whose output an earlier exit discarded;
   - **effect races**: two chains in flight at once, touching one path, URL or object, at least one
     writing;
   - **unexplained effects**: effects of calls the static analysis classified as reads or left
     unclassified, with the call site, ready to paste into `effects(..)`.

A cheaper variant is **differential tracing**: run the test suite with `PSYNC_GRAPH=off` and with
psync, record effect traces, and require that each resource sees the same sequence of operations.
The end-to-end benchmarks already did this informally ("both builds sent the same API requests").

**Result.** Every gap in the static model becomes a test failure with a precise report, instead of
a silent behaviour change.
**Cost.** A Linux-only shim for full coverage. Middleware alone is portable.

## 4. HTTP semantics

**Problem.** A service call is a read or a write, nothing finer. HTTP says more.

**Idea.** When a call names an HTTP method or uses a builder with a method argument
(`request(Method::PUT, url)`), use RFC 9110's classes:
- GET, HEAD and OPTIONS are *safe*: they behave as reads;
- PUT and DELETE are *idempotent*, which makes retries safe, though not running them when
  sequential code would not have;
- POST and PATCH are neither.

When the URL is a literal or a `format!` with a literal prefix, use its host and path as the target,
so writes to different hosts or collections do not conflict.

Idempotency is **not** the same as being safe to speculate. Deleting an object the sequential
program would have kept is still wrong. So idempotency only loosens *ordering* among repeated
identical writes, never the speculation rule.

## 5. Staging speculative writes

**Problem.** A write must wait for every earlier exit, even when its result is not needed until
much later.

**Idea.** For writes whose result only feeds `?`, use write buffering as in thread-level
speculation:
- **files**: write to a temporary file in the same directory when the chain runs, then `rename` it
  into place when the exits before it are replayed, or delete it if one of them leaves;
- **services with transactions or idempotency keys**: prepare the operation, then commit it at the
  replay point;
- **channels**: queue the message locally and send it at the replay point.

**Result.** The slow part (uploading the bytes) overlaps earlier work, and the visible part (the
rename or commit) still happens in program order, and only if sequential code would have done it.
**Cost.** Only some APIs offer a prepare/commit split, and it changes when errors surface:
a rename can fail after the upload succeeded.

## 6. Cancellation instead of completion

Today a join runs every chain to completion, and a loop runs every item's prefix, even after an
earlier exit. Dropping the futures of chains that an exit has made pointless would bound wasted
work. But cancelling a future in the middle of a write leaves partial state: a half-written file,
or a request the server got without the client reading the reply. So cancellation is only safe for
chains with no write effects, which the effect analysis can now tell. A follow-up: cancel
read-only chains early, and keep running chains that write.

## 7. Processes, the environment and the working directory

- **Arguments of `Command`.** `git config --get`, `ls` and `cat` read. Recognizing read-only
  subcommands of common tools would stop every process from conflicting with everything.
- **Working directory.** `env::set_current_dir` changes what every *relative* path means. Today it
  is an environment write. It should conflict with every file effect whose path is not absolute.
- **Environment variables** are process-global, shared with other threads that psync does not
  see. A write to them should be a barrier, not just an ordered effect.

## 8. Resolved types and calls (proposal Phase I, step 3)

All of the above still works from tokens. A rustc driver that runs after type checking (on THIR or
MIR) would know:
- the exact callee of every call;
- which arguments are `&mut`;
- whether a `&self` method uses interior mutability (`Cell`, `RefCell`, `Mutex`, atomics);
- the callee's own effects, transitively, as in the MOD/REF analyses that ReSynchronizer computes
  for JavaScript.

psync would keep the macro as the user interface and the code generator. The driver would refine
the graph and fail the build when the macro's token-level guess was too optimistic.

**Status: built.** [`cargo psync check`](./check.md) is this driver. It resolves callees,
summarizes effects through function bodies and across crates, and reports conflicts, speculative
writes and shared interior-mutable state in the generated code.
[`cargo psync build`](./facts.md) feeds its facts back into the macro, which then schedules with
them. It also resolves `dyn` calls by link-time class-hierarchy analysis and treats explicit panics
as exits. Still open: `&mut`-precise argument effects (which parameter a callee writes through),
and interior mutability as an effect rather than a note.

## 9. A machine-checked argument

The correctness argument is small enough to formalize. Model a region as statements with
read/write sets over variables and effect targets, plus an exit flag. Then prove in Lean that every
schedule which
- respects def-use and conflict edges,
- never starts a write before the exits preceding it, and
- replays exits in program order

produces the same final variable state, the same exit, and the same per-target sequence of effects
as sequential execution. The proof needs commutation axioms for non-conflicting effects, which are
exactly the assumptions this chapter lists. Formalizing it would turn "Assumed by you" in
[What is checked, what is assumed](../internals/soundness.md) into the precise hypotheses of a
theorem.

**Status: the scheduling part is proven.** `verify/Schedule.lean` proves commutation, the
correctness of every level's join under any interleaving, the correctness of the whole schedule,
equivalence with program order for reorderings past independent operations, and the harmlessness
of read-only speculation, and early-exit replay (`replay_correct`: recording every outcome and
replaying the first exit in program order gives the sequential state and exit). Modeling the
replay found a real bug: an exit was replayed before an earlier statement in a later level had
run. Fixed, with a test.

**The outside world is modelled too.** `verify/World.lean` adds a server, as a labelled transition
system, and proves that the schedule preserves the server's state and every response, relative to
a commutativity specification. It also proves that psync's built-in rule is such a specification,
and that the rule is unsound for a server whose reads change state
([What psync guarantees about the outside world](./world.md)).

**Schedules are certified.** psync writes a certificate per concurrent region, and a Lean checker,
proven sound, accepts or rejects it; CI checks all of the repository's schedules
([Certified schedules](./certificates.md)). The checker's hypothesis, that footprints are right,
is proved for a fragment of MIR with cross-function summaries (`verify/Footprint.lean`). Next:
specifications for HTTP, the Kubernetes API and S3, checked against server logs, then
certificates for prefetched loops and exits.

## Priorities

| Step | Closes | Effort |
|---|---|---|
| 1. runtime target checks | the duplicate-item assumption, computed paths | small: code generation only |
| 3. effect sanitizer (middleware + differential traces) | everything the static model misses, in tests | medium |
| 2. type-checked effect contracts | lying names | medium, ongoing per ecosystem |
| 4. HTTP semantics | coarse service conflicts | small |
| 7. processes and cwd | over-conservative processes; cwd hazard | small |
| 5. staged writes, 6. cancellation | wasted work, writes waiting on exits | medium to large |
| 8. compiler driver (built: `cargo psync check`, `build`, `fix`; per-argument effects for synchronous helpers; synchronous levels checked) | the token-level foundation | next: per-argument effects through `async fn` coroutines; exit positions in the checker; paths computed by `join` in MIR |
| 9. Lean model (scheduling built: `verify/Schedule.lean`) | the token-level foundation | exit replay, the server model, certified schedules and footprint soundness (with real compiler output through Charon) built |
