# Dynamic dispatch and class hierarchies

The effect summaries of the [last chapter](./effects.md) need every call resolved to the
function it runs. For most calls, the compiler already knows. For a call through a trait object,
it does not, *by design*. This chapter explains why, and how psync answers the question anyway.

## Static dispatch and monomorphization

```rust,ignore
trait Store { fn checkpoint(&self) -> io::Result<()>; }
struct FileStore;
impl Store for FileStore { fn checkpoint(&self) -> io::Result<()> { fs::write("state.db", b"x") } }

fn save_static(s: &FileStore) { s.checkpoint(); }           // 1. concrete type
fn save_generic<S: Store>(s: &S) { s.checkpoint(); }        // 2. generic
fn save_dyn(s: &dyn Store) { s.checkpoint(); }              // 3. trait object
```

1. In `save_static` the receiver's type is known, so the call resolves to
   `<FileStore as Store>::checkpoint`. This is **static dispatch**.
2. In `save_generic`, `S` could be any type. rustc compiles a separate copy of `save_generic`
   for each type it is used with: `save_generic::<FileStore>`, `save_generic::<MemStore>`. This is
   **monomorphization**. In each copy, the call is static again.
3. In `save_dyn`, `&dyn Store` is a pointer to the data plus a pointer to a **vtable**, a table of
   function pointers that is filled in for the actual type when the reference is made. The call
   jumps through the table. This is **dynamic dispatch**. Nothing in `save_dyn`'s code says which
   implementation will run, and it may differ from call to call.

## What the driver sees

In MIR, all three are `Call` terminators naming the *trait method* `Store::checkpoint`, with
generic arguments. The driver asks rustc to resolve each one:

```rust,ignore
let instance = Instance::try_resolve(tcx, typing_env, callee, generic_args).ok().flatten();
let unresolved = match instance {
    Some(i) => matches!(i.def, InstanceKind::Virtual(..)),   // a vtable call: `dyn`
    None => true,                                            // a generic it cannot pick yet
};
```

- In `save_static`, it resolves to `<FileStore as Store>::checkpoint`, a local body to summarize.
- In `save_generic`, the driver analyses the generic body *once*, before monomorphization, so `S`
  is still a parameter and the call cannot be resolved (`None`).
- In `save_dyn`, it resolves to `InstanceKind::Virtual`: "look it up in the vtable".

So cases 2 and 3 leave the same question open: *which implementations of `Store::checkpoint` can
this call reach?*

## Class-hierarchy analysis

Compilers for object-oriented languages met this question long ago, for virtual method calls.
The classic answer is **class-hierarchy analysis** (CHA). A call of method `m` on a receiver of
type `T` can only reach implementations of `m` in `T` and the types derived from it. So collect
every implementation in the program and take that set as the possible targets. In Rust
the "hierarchy" is flat: a call of `Trait::m` can reach any `impl Trait for X` in the program.

CHA needs to know *every* implementation. That is a **closed-world assumption**: no code
outside what was analysed can add one. Here psync meets a problem particular to Rust. When a
crate is compiled, rustc sees only the crate and its dependencies. The implementation may live
in a crate that depends on the caller, compiled *later*:

```text
datafusion-catalog    ListingSchemaProvider::refresh calls self.factory.create(..)
                      on an Arc<dyn TableProviderFactory>
        ▲ depends on
datafusion (core)     impl TableProviderFactory for ListingTableFactory { fn create .. }
```

No single compilation sees both. So the driver puts the question off until the whole build has
been compiled, and answers it at **link time**:

1. **Defer.** An unresolved call of a trait method from the user's workspace is recorded in the
   summary as *deferred*, keyed by the `DefPathHash` of the trait method, a stable name for
   `Store::checkpoint` that every crate computes the same way.
2. **Record implementations.** Each crate's summary file lists which of its methods implement
   which trait methods.
3. **Link.** After the build, `psync_check::link` takes, for every deferred trait method, the union
   of the summaries of all its implementations in the build. It does this transitively, because an
   implementation may itself defer further calls.
4. **Use.** The result goes to the macro, in the facts file. It also goes back to the driver:
   `cargo psync` writes it to `linked.json` and points the environment variable `PSYNC_LINKED` at
   it. When the resolution changed, `cargo psync` then runs the check a second time, and that
   pass reports what only `dyn` resolution reveals.

The `summaries` example shows the table and the result:

```text
class hierarchy: {"Store::checkpoint": ["FileStore::checkpoint", "MemStore::checkpoint"]}
  snapshot  {"write file state.db"}
```

`snapshot` calls `Store::checkpoint` through `dyn`. One implementation writes `state.db` and the
other does nothing, so a call *may* write `state.db`. The fixture has the real thing, with the
file called `store.db`:

```rust,ignore
#[graph]
pub async fn snapshot(store: &dyn Store) -> Result<usize, Error> {
    store.checkpoint().await?;          // FileStore's implementation writes store.db
    let n = inspect_db().await?;        // reads store.db
    Ok(n)
}
```

```text
warning: psync runs this concurrently with other work that has a conflicting effect: this does
         `read file "store.db"` (via inspect_db -> std::fs::read), the other does
         `write file "store.db"` (via Store::checkpoint -> std::fs::write)
```

## Where the closed world ends

The assumption is only as good as the build. The driver treats these cases as **opaque**, not
as "no effects":

- **Traits of published crates.** `dyn AsyncWrite` or `tower::Service` can be implemented by any
  crate, so CHA over your build proves nothing about them. The driver classifies the common I/O
  traits by the receiver's type instead (`AsyncWrite` on a real file or socket writes it).
- **No implementation in the build.** A trait with no implementation is a sign that one comes from
  elsewhere: a plugin, or a caller in another workspace.
- **Function pointers and closures passed in as values.** These have no trait method to look up.

Facts are therefore valid *for the build they were computed in*. A different set of crates, or
different features, gives different facts, and `cargo psync build` recomputes them.

CHA is also imprecise in a way you can see. `save_generic::<FileStore>` gets the union over *all*
implementations, `MemStore` included, because the driver analyses the generic body once, not
each copy. That costs precision, never soundness. Analysing each monomorphized copy separately
would be more precise and much slower. Finer analyses narrow the set further. *Rapid type analysis* keeps only the implementations of
types the program actually constructs somewhere. *Points-to analysis* tracks which values can
flow to the call.

## Exercises

1. In the `summaries` example, add a third implementation, `NetStore::checkpoint`, that sends
   over the network. What does `snapshot`'s summary become? Is that sound for a program that only
   ever passes a `FileStore`? Is it precise?
2. Why does the driver key deferred calls by `DefPathHash` and not by `DefId`?
3. A crate has `fn run(f: fn() -> io::Result<()>) { f().unwrap() }`. What does its summary say,
   and why can CHA not help?
4. Why is it safe for the link step to take a *union* over implementations, but unsafe for it to
   pick any single one?

<details>
<summary>Answers</summary>

1. `{write file state.db, write net}` (plus whatever else `NetStore` does). It is sound: it includes
   everything that could happen. It is not precise for that program, which can never reach
   `NetStore`. Only a whole-program analysis of which types reach the call could tell.
2. A `DefId` is a crate *number* and an index, and crate numbers are assigned per compilation.
   The same trait method gets different `DefId`s in different crates' compilations. A
   `DefPathHash` is computed from the definition's path and its crate's stable identity, so it is
   the same everywhere, which a key shared between compilations needs.
3. "Call through a function pointer", which is opaque, and it panics (`unwrap` is an explicit
   panic). The callee is a runtime value with no trait method to look up, so there is no hierarchy
   to analyse.
4. The program may run any of them, so a sound summary must include the effects of every one. A
   single one would be right only for the programs that happen to pass that type.

</details>
