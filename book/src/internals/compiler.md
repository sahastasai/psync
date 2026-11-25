# From MIR to summaries across crates

`#[psync::graph]` sees the tokens of one function. When that function calls `helpers::stamp(&d)`,
the macro knows only the name `stamp`. The macro classifies a call by the verb its name starts
with (`get` and `list` read, `put` and `delete` write; [Effects](../safety/effects.md) has the
list), and `stamp` is not one, so it guesses "reads". The body of
`stamp`, in another crate, writes a file. This chapter explains how psync gets the truth from the
compiler: what a **summary** is, how `psync-driver` computes one from MIR, how summaries cross
crate boundaries, how the link step resolves `dyn` calls over the whole build, and how the result
reaches the macro as **facts**.

It assumes [Inside rustc: HIR, THIR, MIR](../foundations/rustc.md), which shows the three forms
on a small function. Every example below is real output, from the test fixture in
`psync-check/tests/fixtures/hidden` (`cargo psync build` on it).

## What a summary is

A **summary** of a function is a short description of everything the function may do to the
world, through every function it calls, without its body. In psync it records four things:

| Field | Meaning | Example |
|---|---|---|
| `effects` | what it may read or write outside memory: a **domain** (files, network, environment, processes, output, or a service such as `kube::Api`), an **access** (read, write, or *ordered* write: a write that must keep its order with every other ordered write of the domain, whatever the targets, such as printing) and a **target** when known (`"store.db"`) | `Fs Write "cache.json" via std::fs::write` |
| `opaque` | calls it could not see through: a function pointer, or a `dyn` method of a trait from a published crate (anything may implement `AsyncWrite`) | `call through a function pointer` |
| `panics` | whether it may panic explicitly (`unwrap`, `expect`, `panic!`, `assert!`) | `false` |
| `deferred` | methods of your workspace's traits it calls through `dyn` or an unresolved generic, left for the link step | the `DefPathHash` (a stable name, explained below) of `Store::checkpoint` |

Here is the whole summary file of the fixture's `helpers` crate:

```json
{
  "crate_name": "helpers",
  "crate_id": "StableCrateId(11486981337079494651)",
  "functions": {
    "DefPathHash(Fingerprint(11486981337079494651, 10293074283500572899))": {
      "path": "stamp",
      "effects": [{ "domain": "Fs", "access": "Write", "target": null, "via": ["std::fs::write"] }],
      "opaque": [], "panics": false, "deferred": []
    },
    "DefPathHash(Fingerprint(11486981337079494651, 1203218390450643680))": {
      "path": "inspect",
      "effects": [{ "domain": "Fs", "access": "Read", "target": null, "via": ["std::fs::read"] }],
      "opaque": [], "panics": false, "deferred": []
    }
  },
  "impls": {},
  "calls": {}
}
```

`stamp`'s target is `null` because it writes `format!("{dir}/.synced")`, a path computed at run
time. An unknown target conflicts with every target in its domain.

An effect can also name a **parameter** instead of a target. In

```rust,ignore
fn write_it(p: &Path, d: &str) -> io::Result<()> { std::fs::write(p, d) }
```

the effect is `{ "domain": "Fs", "access": "Write", "target": null, "arg": 0 }`: it writes the
file passed as parameter 0. Each caller replaces `arg` by what it passes there. A string constant
becomes the target, so `write_it("a.json".as_ref(), ..)` writes `"a.json"`. The caller's own
parameter becomes the caller's `arg`. Anything else becomes unknown.

**The one property a summary must have is over-approximation.** Every effect the function can
really have must be covered by an effect in its summary. A summary may say too much, which only
makes psync more careful. It may never say too little, which would let psync run two conflicting
calls at once. The proof in `verify/Footprint.lean` states this exactly for a fragment of MIR:
a summary table is **valid** if each function's summary covers the footprint of its own body,
reading calls from the table. If the table is valid, every statement changes nothing outside the
writes of its footprint (`frame`), and its result depends only on what the summary says it touches
(`dep`), where a statement's footprint is computed from the summaries of what it calls. See [Certified schedules](../safety/certificates.md#footprints-proved-for-a-fragment-of-mir).

## Where psync enters the compiler

`cargo psync check` runs an ordinary `cargo check`, with one change: it sets `RUSTC_WRAPPER` to
`psync-driver`. Cargo then runs the driver wherever it would have run `rustc`, once per crate, in
dependency order. The driver *is* rustc, linked as a library (`rustc_driver`), plus one callback:

```text
 source ──parse──▶ tokens/AST ──expand macros──▶ AST ──resolve, lower──▶ HIR
   (#[psync::graph] runs here, on tokens)                                  │ type check
                                                                           ▼
                        borrow check ◀── MIR (built) ◀──build── THIR (every expression typed)
                              │
                              ▼
                      after_analysis(tcx)  ◀── psync-driver's callback
                              │ tcx.optimized_mir(f) for each function
                              ▼
                    summaries, checks, facts   ──▶ compilation continues (codegen, under cargo build)
```

In code (`psync-check/src/bin/psync-driver.rs`):

```rust,ignore
impl rustc_driver::Callbacks for Psync {
    fn after_analysis<'tcx>(&mut self, _: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let mut analyzer = analysis::Analyzer::new(tcx);
        let mut checker = analysis::Checker::new(&mut analyzer);
        checker.run();              // compare #[psync::graph] levels with the summaries
        checker.write_findings();
        analyzer.write_summaries(); // target/psync/summaries/<crate>-<id>.json
        Compilation::Continue
    }
}
```

`after_analysis` runs only once type and borrow checking have succeeded, so every type is known
and every call is resolved as far as the compiler can resolve it. The macro has already run, so
the driver sees the code `#[psync::graph]` generated. That code wraps each chain of a **level**
(the chains psync runs at once) in a call such as `maybe_done_at::<LEVEL, ORDER, ..>` (a future),
or `job_at::<LEVEL, ORDER, ..>` (a job of synchronous code), whose
constant arguments record what the macro assumed. These **markers** are how the driver finds the
levels to check ([`cargo psync check`](../safety/check.md#1-markers-in-the-generated-code)).

The diagram draws the forms in the order each depends on the previous one. rustc does not run
them as fixed passes, though: it computes each on demand, as the next section explains.

### Queries

rustc is not a sequence of passes. It is a database of **queries** on a central context,
`TyCtxt`, usually named `tcx`. `tcx.optimized_mir(def_id)` asks for a function's optimized MIR;
rustc computes it on first use (building THIR, then MIR, then running the MIR passes) and caches
it. The driver asks for little else:

| Query | What psync uses it for |
|---|---|
| `tcx.hir_body_owners()` | every function, closure and constant with a body in this crate |
| `tcx.optimized_mir(def_id)` | the body to walk |
| `tcx.is_mir_available(def_id)` | whether a body exists here (another crate's non-generic function has none) |
| `Instance::try_resolve(tcx, env, callee, args)` | which function a call really runs: a trait method on a known type resolves to its impl |
| `tcx.def_path_hash(def_id)`, `tcx.stable_crate_id(krate)` | stable names for summary files |
| `tcx.trait_of_assoc(def_id)`, `tcx.associated_item(..).trait_item_def_id()` | which trait a method belongs to, and which trait method an impl method implements |

Why optimized MIR, and not the HIR or THIR? MIR is the form where a function is a graph of
simple statements and **calls are terminators** with an explicit callee. `?`, `for`, `.await`
and method syntax are all gone. A call `fs::write("cache.json", text)` is one terminator:
`_4 = std::fs::write::<&str, String>(const "cache.json", move _5)`. Walking every block's
terminator visits every call the function can make, and nothing else.

## Names that survive between compilations

Inside one compilation, a definition is named by a **`DefId`**: a crate number and an index into
that crate's definitions. Crate numbers are assigned per compilation, so the `DefId` of
`helpers::stamp` differs between compiling `helpers` and compiling `app` (crate `0` is always the
crate being compiled). The index is no better as a name: it depends on the order of definitions,
so adding a function shifts it, and two versions of a crate can share a name. A summary file written
during one compilation must be read in another, so it needs a name that is the same in both.

That name is the **`DefPathHash`**: a 128-bit hash of the definition's full path (`helpers` →
`stamp`), whose first half is the crate's **`StableCrateId`**, itself a hash of the crate name and
its `-C metadata` (which cargo makes unique per package version). In the file above, both keys
start with `11486981337079494651`, the `crate_id` of `helpers`. rustc uses the same hashes for
incremental compilation, for the same reason.

```text
  compiling helpers                          compiling app
  ─────────────────                          ─────────────
  DefId(0:4) = stamp                         DefId(18:4) = helpers::stamp
        │ def_path_hash                             │ def_path_hash
        ▼                                           ▼
  DefPathHash(11486981337079494651, 10293074283500572899)   ← the same key
        │                                           │
        └──▶ helpers-StableCrateId(...).json ───────┘ lookup
```

## Inside one crate: walking MIR

For each function, `Analyzer::local` walks the optimized MIR once:

- each `Call` terminator with a known callee goes to `Analyzer::call`, which first runs
  `Instance::try_resolve`, so `<FileStore as Store>::checkpoint` is found when the type is known;
- a call of `std`, `core`, `tokio` or similar is classified **by path** (`std::fs::write`
  writes a file; a string constant argument becomes the target);
- a call of a function in the same crate becomes an **edge** to that function, followed later;
- a statement `_0 = {closure@..}` or `{coroutine@..}` is also an edge: an `async fn` returns a
  coroutine, and its body is where the work is;
- a call through a function pointer is **opaque**.

Per-argument effects need to know what each argument of a call holds. `sources` reads every local
of the body once:
- A parameter is followed only if it is never assigned again and never borrowed mutably. Then it
  still holds what the caller passed.
- Any other local is followed only if it is assigned exactly once, from a copy, a move or a
  shared borrow of a followed local (through `*`), from a string constant, or from a
  standard-library conversion that keeps the value (`as_ref`, `deref`, `to_path_buf`, `into`, ...)
  of one.
- Anything else, such as `p.push("x")` on a copy of `p`, is unknown.

An edge records what its call passes, and absorbing a callee's summary substitutes it. The
link step and `dyn` calls clear `arg`: the parameter is the implementation's, not the call's.

An `async fn` keeps its parameters in its coroutine. The function's body only builds the
coroutine, `_0 = {coroutine} { path: copy _1, data: copy _2 }`, and the coroutine's body reads them
as its **captures**, `copy ((*_7).0)`, where `_7 = copy (_1.0)` is the pointer inside the
`Pin<&mut Self>`. Across an `await` they are first saved in a slot of the suspended state,
`((*_7) as variant#3).0`, and read from there. `sources` therefore follows three kinds of values:
locals, captures and slots. In a closure or coroutine, `Param(k)` means capture `k`, and the edge
from the body that builds it passes the sources of its operands. A slot is followed like a local
that is assigned once.

Awaiting the future reaches the coroutine a second time, through `Future::poll`, whose argument
is the `Pin` and says nothing about the captures. That edge would add the same effects with an
unknown target. It is skipped when the polled value comes, through `into_future`,
`Pin::new_unchecked`, borrows, moves and slots, from a call in the same body of the `async fn`
that builds that coroutine, because that call already brought its effects with the arguments
substituted. A future that comes from anywhere else, such as a parameter, keeps the edge.

`keep` in the fixture writes after an `await`:

```rust,ignore
async fn keep(path: &str, data: &str) -> std::io::Result<()> {
    ready().await;
    std::fs::write(path, data)
}
```

Its summary is `Fs Write arg 0`. `keep("x.json", ..)` and `keep("y.json", ..)` are two different
files (`keep_both`, accepted), and `keep("x.json", ..)` twice is a conflict (`keep_same`). An
async closure's body is another coroutine, built by the closure. It is not followed yet.

The edges form a call graph, and a function's summary is its own effects plus the summaries of
everything it calls:

```text
S(f) = own(f) ∪ ⋃ { S(g) : f calls g }
```

Recursion makes the graph cyclic, so `summarize` groups functions into strongly connected
components with Tarjan's algorithm (callees before callers) and iterates each component until no
summary grows. Summaries only grow and there are finitely many effects, so this ends, and every
member of a cycle gets the effects of the whole cycle. This is the least solution of the equation
above. It is also exactly what `Valid` in `Footprint.lean` checks: each summary covers its body
with the calls read from the summaries.

## Across crates: one file per crate

When cargo compiles `app`, `helpers` was compiled before it, by the same driver, which wrote
`target/psync/summaries/helpers-StableCrateId(11486981337079494651).json`. A call from `app` to
`helpers::stamp` cannot be followed into a body, because `app`'s compilation does not have
`stamp`'s MIR. rustc stores a function's MIR in the crate's metadata only when other crates
need it to generate code: generic functions, which are compiled anew for each type they are used
with, and `#[inline]` functions, whose body is copied into the caller. An ordinary function is
compiled once, in its own crate, and only linked. Even for a generic callee, the driver reads the
callee's summary rather than its MIR, so that every crate is analysed once. The driver looks
the call up instead:

```text
  app's MIR:  _7 = helpers::stamp(move _8) -> ...
                     │
                     │ def_path_hash
                     ▼
  summaries/helpers-*.json ── functions[DefPathHash(1148…, 1029…)] ── Fs Write (via std::fs::write)
                     │
                     ▼
  sync_dirs:  Fs Write, via helpers::stamp -> std::fs::write
```

A function missing from a summarized crate's file has no effects: the file omits functions whose
summary is empty, and `crate_id` tells the driver the crate *was* summarized. For a crate with no
file, the driver falls back to the macro's rule. A call from your code to an `async` function
named by a verb is classified by the verb, and any other call has no effects.

Two rules keep library summaries honest. Inside crates.io and git crates, verbs are never used:
only real I/O counts (files, environment, processes, output). Socket traffic is left out there,
because inside an HTTP client every request, a `GET` too, is a socket write. Verbs apply only at
the boundary, to an `async` function of a published crate called from your workspace, as
[`cargo psync check`](../safety/check.md#2-effect-summaries-from-mir) explains.

## The link step: `dyn` across the whole build

A call through `dyn Store` cannot be resolved while compiling the caller, because an
implementation may live in a crate compiled later. The fixture's `snapshot` calls
`store.checkpoint()` on a `&dyn Store`:

```rust,ignore
pub trait Store {
    fn checkpoint(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_>>;
}
impl Store for FileStore {
    fn checkpoint(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + '_>> {
        Box::pin(async { std::fs::write("store.db", b"x") })
    }
}
async fn inspect_db() -> io::Result<usize> {
    Ok(std::fs::read("store.db")?.len())
}

#[graph]
pub async fn snapshot(store: &dyn Store) -> Result<usize, Error> {
    store.checkpoint().await?;
    let n = inspect_db().await?;
    Ok(n)
}
```

The tokens say nothing about `store.db`, so the macro puts the two awaits in one level. The
driver records the call as **deferred**, keyed by
the `DefPathHash` of the trait method `Store::checkpoint`, and each crate's file lists which of
its methods implement which trait methods (`impls`).

After the build, `cargo psync` runs the **link step** (`psync_check::link`) over all summary
files at once:

```text
  every crate's file ─┐
                      ├─▶ link: for each deferred trait method t,
  impls: t → [i, …] ──┘        effects(t) = ⋃ summary(i) over implementations i,
                               following their own deferred calls (transitively)
                                        │
                     ┌──────────────────┴──────────────────┐
                     ▼                                     ▼
              linked.json (for the driver)          facts.tsv (for the macro)
```

This is class-hierarchy analysis over a closed world: the build. A second `cargo check` pass
gets `linked.json` through `PSYNC_LINKED`, and now `store.checkpoint()` writes `store.db`. Two effects
**conflict** when one writes and both may reach the same resource (same domain, targets that may
be equal), so the driver reports a conflict between it and `inspect_db()`, which reads `store.db`,
in the same level. After that pass the link step runs again, because the summaries now include
the resolved effects, and `facts.tsv` must be written from them.

Only traits of your workspace are deferred. A `dyn` call of a published crate's trait
(`AsyncWrite`, `tower::Service`) is opaque: its implementations need not be in the build at all. The driver registers
`PSYNC_LINKED_STAMP` as an environment dependency of each workspace crate, so cargo rechecks a
crate exactly when the linked resolution changes. A plugin implementing `Store` outside the build
is the closed world's limit.

## Facts: back to the macro

The macro runs before any of this, on tokens. To let it schedule with what the compiler knows,
the driver also writes one **fact** per call site inside a `#[psync::graph]` function (the
`calls` field of the summary file, keyed by `file:line:column`), and the link step turns them,
with `dyn` calls resolved, into `facts.tsv`. One line per call:

```text
app/src/lib.rs:23:5    warm        0  0  fs␟w␟cache.json
app/src/lib.rs:51:9    stamp       0  0  fs␟w␟
app/src/lib.rs:110:11  checkpoint  0  0  fs␟w␟store.db
```

The columns are the call site (file, line and column of the callee's name), the name as written,
whether it may panic, whether some callee was opaque, and the effects (`␟` separates domain,
access and target; an empty target is unknown). A fourth field `j` says the target is the call's
argument `j`, where 0 is a method's receiver. The macro then takes the target from the argument
as written: `write_and_make_executable(&apprun, &data)` writes whatever `apprun` names. `cargo psync build` runs the compiler with `PSYNC_FACTS` set to the
file's path, and the macro opens and reads it. It does so twice: once under `cargo check`, so the
driver can check the schedule built from the facts, and finally under `cargo build`, which
produces the program. The macro computes the same `file:line:column` key from its
tokens' spans, and uses a fact only if the name there still matches, so a fact from before an
edit is ignored, never misapplied. The generated code reads `option_env!("PSYNC_FACTS_STAMP")`,
which makes cargo rebuild an annotated crate when the facts change.

The whole loop:

```text
 cargo psync build
   1. cargo check, RUSTC_WRAPPER=psync-driver   every crate, in dependency order:
                                                summaries + impls + call facts → summaries/*.json
   2. link                                      → linked.json, facts.tsv
   3. cargo check again, PSYNC_LINKED           only if step 2 resolved deferred calls: the
                                                driver re-checks with them, then links again
   4. cargo check, PSYNC_FACTS                  the macro schedules with facts; the driver
                                                checks that schedule too
   5. cargo build, PSYNC_FACTS                  the real build, the same schedule
```

## Dependence graphs: which branch decides a call

A summary says what a function may do. It does not say when. To find a function's critical path
under each condition (board item #168), psync also needs to know which branch decides whether
each call runs. That is **control dependence**, the second half of the program dependence graph
([Related work](../reference/related.md#the-program-dependence-graph-ferrante-ottenstein-and-warren-1987)).
After every `check`, the driver writes one for each `#[psync::graph]` function and local `async
fn`, and for the closures and async blocks in them, to `target/psync/pdg`.
`cargo psync pdg FUNCTION` prints them. For the fixture's `guarded`:

```rust,ignore
pub async fn guarded(path: &str) -> std::io::Result<()> {
    let n = std::fs::read_to_string(path)?.len();
    ready().await;
    if n > 3 {
        std::fs::write(path, "long")?;
    }
    Ok(())
}
```

```text
guarded::{closure#0}
  b29  line 253  call std::fs::read_to_string  [read file (parameter 0)]
  b7   line 254  call ready  if b2 (line 253, `?`) = ok
  b13  line 254  await  if b2 (line 253, `?`) = ok
  b16  line 256  call std::fs::write  [write file (parameter 0)]  if b15 (line 255, if) = true
```

Each line is a point of interest: a call with effects or into the crate, or an await. It shows the
MIR block, the source line, and the branches the point depends on. `guarded::{closure#0}` is the
coroutine that holds the `async fn`'s body. "Parameter 0" is its first capture, `path`.

**The algorithm.** Block `n` is control dependent on the edge `a -> b` when every path from `b`
to the exit passes `n` (`n` post-dominates `b`), and not every path from `a` does. Post-dominators
are dominators of the reversed graph, computed from a virtual exit with Cooper, Harvey and
Kennedy's iterative algorithm. Then, for each edge out of a branch, the walk goes up the
post-dominator tree from the edge's target to the branch's own immediate post-dominator. Every
block on the way depends on that edge (`psync-check/src/pdg.rs`, with unit tests for a diamond,
an early return, a loop and a loop without exit). Dependence is direct. The write above depends
on the `if`, and the `if` on the `?`, so the write runs only when both allow it.

**Undoing the state machine.** `optimized_mir` of an async body is already a state machine. It
starts with a switch on the saved state, and each `.await` stores a state and returns `Pending`.
Read literally, everything would depend on that first switch (`switch = 0`, as the test shows
when the undo is removed). Before computing dependence, the driver rebuilds the source-level flow:
- the state switch is not a branch, and the first block goes only to the unresumed state's
  target;
- each suspension (`SetDiscriminant(state k)` followed by `return`) continues at the block the
  switch sends state `k` to;
- the `Poll` switch of an `.await`'s own polling loop is not a branch, and an await takes the
  guards of the poll before it.

Edges into cleanup blocks are left out, so unwinding is not a branch. Labels come from the
switched value. An enum's variant names it (`Some`, `Err`), a `bool` is `true` or `false`, and a
`?` (its `ControlFlow` switch, marked by the desugaring) is `ok` or `exits`.

**Not yet.** Data edges between the points (which result flows into which call), the
spawn/join/select structure of the macro's joins, and summaries of loops. Effects are written in
terms of the body's own parameters. For a closure or async block, those are its captures.

## What is proved, and what is assumed

- **Proved** (`verify/Footprint.lean`), for a fragment of MIR with places, assignments, `if`,
  `while` and calls by reference: valid summaries over-approximate, and a schedule whose
  certificate covers them equals the sequential program.
- **Assumed** for real Rust: that the driver's classification of `std` and tokio calls by path is
  right; that MIR's references, interior mutability and `unsafe` behave as the fragment's places
  do, which the borrow checker makes true for safe code; that opaque calls are reported, not
  missed; and the closed world of the link step.

## Exercises

1. Run `cargo psync build` on `psync-check/tests/fixtures/hidden` (with `CARGO_TARGET_DIR` set),
   and find `stamp`'s key in `helpers-*.json`. Why are its first 64 bits the same as the file's
   `crate_id`? Then find the call of `stamp` in `app-*.json`'s `calls`: which effects did it get?
2. Add a second implementation of `Store` that only reads `store.db`. What does the link step
   give for `Store::checkpoint` now, and does the conflict in `snapshot` remain?
3. Why can `app`'s compilation not simply call `tcx.optimized_mir` on `helpers::stamp`? (Hint:
   `tcx.is_mir_available`.) Which functions of `helpers` would have MIR in `app`'s compilation,
   and why does the driver still use their summaries?
