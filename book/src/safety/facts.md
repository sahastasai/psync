# Scheduling with compiler facts: `cargo psync build`

[`cargo psync check`](./check.md) tells you where the macro's guesses were wrong.
`cargo psync build` stops it from guessing. The compiler's knowledge of every callee goes back
into the macro, which schedules with it:

```sh
cargo psync build            # facts, check the facts-driven schedule, then cargo build
cargo psync check --facts    # the same, without the final build
cargo psync fix              # insert #[psync::seq] wherever a plain build would be wrong
```

This closes the loop the proposal's Phase I, step 3 describes. The compiler (HIR and MIR, with
types and calls resolved) refines the graph the macro builds from tokens, and separately validates
the schedule that comes out.

## The loop

```text
 1. cargo check + psync-driver        every crate: effect summaries, trait impls, and a fact for
                                      each call site in #[psync::graph] functions
 2. link                              resolve dyn/generic trait calls over the whole build
                                      -> target/psync/facts.tsv
 3. cargo check + driver, PSYNC_FACTS the macro schedules with the facts; the driver checks
                                      the result
 4. cargo build, PSYNC_FACTS          the real build, same schedule
```

A fact belongs to a source location: the file, line and column where the callee's name is written.
The macro computes the same key from its tokens' spans (`proc_macro::Span::line` and `column`). A
fact is used only if the callee's name at that location still matches, so facts from before an edit
are ignored rather than misapplied. The generated code reads `option_env!("PSYNC_FACTS_STAMP")`,
which makes cargo rebuild an annotated crate whenever the facts change.

## What the facts change

For each call with a fact, the macro uses the compiler's effects instead of the name's.

- **Fewer false refusals.** `catalog.create_view("a.sql")` sounds like a write. If its body only
  reads, two calls overlap.
- **Effects hidden in callees.** A helper named `warm()` that writes `cache.json` is a write to
  that file, so it stays ordered with a read of it.
- **Panics are exits.** A callee that panics on purpose (`unwrap`, `expect`, `panic!`, `assert!`,
  `unreachable!`, directly or through the user's own functions) counts as an exit. A write after it
  waits for it, and a loop whose body may panic counts as one that can end early. Panics inside
  libraries are not counted; nearly every library function could panic somewhere.
- **Opaque calls stay conservative.** When some callee could not be seen through, the fact is
  marked opaque. Examples are a `dyn` trait of another crate (`AsyncWrite::poll_shutdown`), a
  function pointer, or a trait with no implementation in the build. The macro then applies the
  compiler's effects *and* the name's guess.

The test fixture shows all of it. Without facts, `cargo psync check` reports 7 warnings and a
note. With facts, the macro produces a schedule with none. Seven functions are kept in order:
`start`, `login`, `snapshot`, `tour` (whose `visit` writes a file through a mutual recursion),
`release` (whose `fs::copy` writes its second path), `boot` (whose `parse_config` may panic before
a write) and the `sync_dirs` loop. One gains concurrency: `views`, whose
`create_view` only reads.

## On real code

| Function | Names only | Compiler facts |
|---|---|---|
| DataFusion `ListingSchemaProvider::refresh` | loop not prefetched (`create` reads as a write) | **loop prefetched**: the `dyn TableProviderFactory::create` resolves to implementations that only read |
| DataFusion writer `shutdown` loop | sequential | sequential: `AsyncWrite::shutdown` on a real writer writes |
| kube-rs `dynamic_api` `main` | client setup overlaps `tracing` init | in order: client setup may run a credential process, and `tracing` init may print |
| the other 8 annotated functions | | unchanged |

Both benchmark branches report 0 warnings with and without facts. A from-scratch check takes 74 s
(kube-rs) and 95 s (DataFusion) on 16 cores. An incremental `check --facts` recompiles only the
annotated crates: 3.4 s on kube-rs and 26 s on DataFusion.

Two lessons from getting there:

- An early version marked `writer.shutdown()` as having no effects. It resolves to tokio's generic
  `AsyncWriteExt::shutdown`, which writes through a `dyn AsyncWrite` the driver never sees. The
  driver now classifies the I/O traits themselves (`AsyncWrite`, `io::Write`, `AsyncRead`,
  `io::Read`) on real readers and writers. It marks any call through another crate's trait object
  as opaque.
- A crate built twice with different features contributes two facts for one site. They are merged
  conservatively: every effect from either, and panicking or opaque if either says so.

## `dyn` calls: link-time class-hierarchy analysis

The compiler can only resolve a trait-object call to its implementations when it sees all of them.
When a crate is compiled, it sees only its dependencies. DataFusion's
`ListingSchemaProvider::refresh` calls `self.factory.create(..)` on an
`Arc<dyn TableProviderFactory>`. Its main implementation, `ListingTableFactory`, is in the
`datafusion` crate, which depends on the crate that makes the call. No single compilation can
see the implementation from the call.

So the driver defers those calls:

1. A call to a trait method through `dyn`, or on a generic the compiler cannot resolve, is
   recorded as *deferred*, keyed by the trait method's stable definition hash. This applies when the
   trait belongs to the user's workspace. Traits of crates.io crates are opaque instead.
2. Every crate also records which of its methods implement which trait methods.
3. After the build, the link step takes the union over every implementation in the build,
   transitively: an implementation may defer further calls. This is class-hierarchy analysis over a
   closed world, the build itself.

The result goes back to the driver too (`PSYNC_LINKED`). A second check pass then reports conflicts
that only `dyn` resolution reveals. In the fixture, `store.checkpoint()` on a `&dyn Store` writes
the file the next call reads.

The closed-world assumption is the limit. A crate outside the build can implement your trait and
be passed in at run time, for example a plugin. Facts are valid for the build they were computed
in. Trait methods with no implementation in the build are treated as opaque.

## `cargo psync fix`

`cargo psync fix` runs the check and inserts `#[psync::seq]` before every statement with a
warning, or before the loop for a loop warning, with the statement's indentation:

```rust,ignore
    #[psync::seq]
    for d in dirs {
        helpers::stamp(&d).await?;
    }
```

On a copy of the fixture it inserts 7 hints, after which `cargo psync check` reports 0 warnings.
`#[psync::seq]` is the safe fix, not always the best one. For idempotent writes,
`#[psync::speculate]` keeps the concurrency. With `cargo psync build`, no hint is needed at all.
