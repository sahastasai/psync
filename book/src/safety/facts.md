# Scheduling with compiler facts: `cargo psync build`

[`cargo psync check`](./check.md) tells you where the macro's guesses were wrong.
`cargo psync build` stops it from guessing. The compiler's knowledge of every callee goes back
into the macro, which schedules with it:

```sh
cargo psync build            # facts, check the facts-driven schedule, then cargo build
cargo psync check --facts    # the same, without the final build
cargo psync fix              # insert #[psync::seq] wherever a plain build would be wrong
```

This closes the loop that step 3 of Phase I ([Where psync is going](../introduction.md#where-psync-is-going)) describes. The compiler (HIR and MIR, with
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

## Exercises

The answers to 1 and 2 come from `cargo psync build` on a one-file crate.

1. ```rust,ignore
   impl Db {
       pub async fn create_view(&self, name: &str) -> io::Result<String> {
           std::fs::read_to_string(format!("{name}.sql"))
       }
       pub async fn get_totals(&self, name: &str) -> io::Result<String> {
           std::fs::read_to_string(format!("{name}.csv"))
       }
   }

   #[graph(explain)]
   pub async fn report(db: &Db) -> io::Result<(String, String)> {
       let view = db.create_view("sales").await?;
       let totals = db.get_totals("sales").await?;
       Ok((view, totals))
   }
   ```

   What does a plain build schedule, and what does `cargo psync build` schedule?
2. Now `create_view` writes `format!("{name}.sql")` instead of reading it. `get_totals` still
   reads `{name}.csv`, a different file. What does `cargo psync build` do, and why?
3. You run `cargo psync build`, then add a line at the top of the file and run a plain
   `cargo build` with the same `PSYNC_FACTS`. Can the old facts be applied to the wrong calls?
4. Your crate defines `trait Store`, and a plugin loaded at run time implements it. What do the
   facts say about `store.checkpoint()` calls, and can they be wrong?

<details>
<summary>Answers</summary>

1. A plain build keeps them in order: `create` is a write verb, so
   ``n0 -> n1 (write `db` "sales" (create_view) vs read `db` "sales" (get_totals))``. With facts,
   both are reads, ``{read file (create_view, per the compiler)}``, and the level is `n0 || n1`.
2. It keeps them in order:
   ``n0 -> n1 (write file (create_view, per the compiler) vs read file (get_totals, per the compiler))``.
   The driver keeps a file target only when it is a string literal. A path computed inside the
   callee is "some file", and a write to some file may be the file the other call reads.
3. No. A fact is keyed by the file, line and column of the callee's name, and is used only if the
   name written there still matches. The shifted calls find no fact (or one for another name)
   and fall back to their names. That is a plain build's schedule, which may be less concurrent,
   or miss what only the facts showed. The stamp makes cargo rebuild when the facts change, but
   not when the source does, so run `cargo psync build` again after editing.
4. The facts take the union of every implementation in the build. The plugin is not in the
   build, so its effects are missing: the closed-world assumption fails. Declare what plugins may
   do (`effects(write(checkpoint))`), or keep such calls `#[psync::seq]`.

</details>
