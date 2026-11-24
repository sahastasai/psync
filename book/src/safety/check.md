# Checking rewrites: `cargo psync check`

`#[psync::graph]` sees only the tokens of the function it rewrites. When it meets
`warm().await` or `session.get_token().await`, it can only go by the name. `warm` has no verb,
so it is assumed to read. `get_token` starts with `get`, so it reads. If their bodies write files,
the macro cannot know.

`cargo psync check` reads those bodies. It runs the compiler with a custom driver, the way
`cargo clippy` does, that sees the program after type checking. It computes what every function
does to the outside world and compares that with the concurrency the macro generated.

```sh
cargo psync check            # warnings, like clippy's
cargo psync check --deny     # findings are errors (exit status 1), for CI
cargo psync check --notes    # also output order and shared Cell/RefCell/Mutex state
cargo psync summaries        # what each function was found to do
```

It does not change the rewrite. It tells you where the rewrite is wrong, so you can fix it with a
hint (`#[psync::seq]`, `#[psync::speculate]`, `effects(..)`).

## An example

`tests/fixtures/hidden` in `psync-check` is a workspace whose rewrites look safe from their tokens:

```rust,ignore
async fn warm() -> std::io::Result<()> {          // no verb: assumed to read
    std::fs::write("cache.json", b"{}")
}
async fn load() -> std::io::Result<String> {
    std::fs::read_to_string("cache.json")
}

#[graph]
pub async fn start() -> Result<String, Error> {
    warm().await?;                                 // these two now run concurrently
    let s = load().await?;
    Ok(s)
}
```

`cargo psync check` reports:

```text
warning: psync runs this concurrently with other work that has a conflicting effect: this does
         `read file "cache.json"` (via load -> std::fs::read_to_string), the other does
         `write file "cache.json"` (via warm -> std::fs::write)
  --> app/src/lib.rs:24:20
   |
24 |     let s = load().await?;
   |                    ^^^^^
   |
note: the conflicting effect
  --> app/src/lib.rs:23:12
   |
23 |     warm().await?;
   |            ^^^^^
   = note: in the code `#[psync::graph]` generated for `start`
```

The same run finds three more:

- `session.get_token()` writes a file although its name reads. psync starts it before `check()?`
  has succeeded (`speculative-write`):

  ```text
  warning: psync starts this before knowing whether an earlier statement exits early, and it does
           `write file "token"` (via Session::get_token -> std::fs::write); ...
    --> app/src/lib.rs:43:27
  note: the earlier work that may exit
    --> app/src/lib.rs:42:5
     |
  42 |     s.check().await?;
  ```

- A prefetched loop calls `helpers::stamp(&d).await?`, a function in *another crate* whose name
  says nothing. Its summary, written when the driver compiled `helpers`, shows a file write. The
  loop can end early at the `?` (`loop-speculative-write`):

  ```text
  warning: psync runs this loop's prefix for every item before the loop body runs, and the body
           can end the loop early, but the prefix does `write file`
           (via helpers::stamp -> std::fs::write). ...
    --> app/src/lib.rs:51:9
     |
  51 |         helpers::stamp(&d).await?;
  ```

- With `--notes`, two concurrent chains that both use one `&RefCell<Vec<u32>>`: their updates now
  interleave (`interior-mutability`).

The fifth rewritten function, which reads two different files, gets no finding.

## How it works

### 1. Markers in the generated code

The macro now tags what it made concurrent. Each chain of a concurrent level is wrapped in
`maybe_done_at::<LEVEL, ORDER, EXITS, SPEC, INDEP, _>(async { .. })` instead of `maybe_done`, and
each prefetched loop prefix in `over_at::<LOOP, TERMINATES, SPEC, INDEP, ..>`. The const
generics record what the macro assumed:
- which level the chain belongs to;
- where it starts in program order;
- whether it may exit early;
- whether it carries `#[psync::speculate]` or `#[psync::independent]`;
- for loops, whether the body can end the loop.

The wrappers are `#[inline(always)]` calls to the old helpers, so they cost nothing at run time.

### 2. Effect summaries from MIR

`psync-driver` is rustc with one extra step after analysis. For every function of the crate it
walks the optimized MIR and classifies every call:

| Callee | How it is classified |
|---|---|
| `std`, `core`, `alloc`, `tokio`, `async-std`, `smol`, `fs-err` | by path: `fs::write`, `File::create` and `<File as Write>::write_all` write files; `fs::read_to_string` reads; `net` sends and receives; `process::Command::output` runs a process; `env::set_var` writes the environment; `println!` writes standard output (in order) |
| a function in the same crate | its body, recursively, including the closures and coroutines it creates (an `async fn`'s body is a coroutine) |
| an `async` function (or one returning a future) in a crates.io or git crate, named with a verb (`delete_object`, `list`), called from your workspace | the verb, as the macro does for awaited calls. The domain is the type or module the function belongs to (`kube::Api::<K>`), not a variable name |
| any other function of another crate | that crate's summary, written when the driver compiled it (no entry: no effects) |
| a function pointer, or `dyn` without a summary | *opaque*: listed by `cargo psync summaries --all` |

Names count only at the boundary between your code and a published API. Inside crates from
crates.io or git, verbs are not used at all. A library's summary keeps only the real I/O it reaches:
files, the environment, processes and output. Socket traffic is left out, because at that level
every request, a `GET` included, is a socket write. Your workspace crates, including path
dependencies, are summarized fully, and calls between them follow the callee's body, never its name.
Verbs also apply only to calls that return a future, as the macro applies them only to awaited
calls: `HashMap::insert` and `Serializer::append_pair` are not I/O. Blocking clients
(`reqwest::blocking`, synchronous database drivers) are therefore judged by their bodies' real I/O
only. Without these rules, in-memory operations look like service writes:
`HeaderMap::try_append` ("append"), `tracing`'s `set_interest` ("set") and flatbuffers'
`run_verifier` ("run"). The first runs on kube-rs and DataFusion reported exactly these.

Calls are resolved exactly: a trait method call resolves to the impl that runs, whenever the types
are known. File paths that are string literals are kept as targets, so `"a.txt"` and `"b.txt"` do
not conflict. Each effect remembers the chain of calls that leads to it (`via warm ->
std::fs::write`).

Summaries are written to `target/psync/summaries/<crate>-<id>.json` for every crate in the build,
dependencies included. Crates are compiled in dependency order, so a crate's callees in other
crates already have summaries when it is analyzed.

### 3. Checks

For every function with markers, the driver groups the chains of each level and compares them in
program order:

| Finding | When |
|---|---|
| `conflict` | two chains of one level have conflicting effects: same domain, one writes, targets may be the same, and neither is `independent` |
| `speculative-write` | a chain writes, an earlier chain of the same level may exit, and the writer is not `speculate` |
| `loop-speculative-write` | a prefetched prefix writes, the body can end the loop, and the loop is not `speculate` |
| `interior-mutability` (note) | two chains capture the same variable whose type, behind references, `Rc`, `Arc` or `Box`, is a `Cell`, `RefCell`, `OnceCell`, `Mutex`, `RwLock` or atomic |
| `output-order` (note) | two chains both print |

Every finding the driver reports is one the macro could not see. When the macro *does* recognize
a conflict, it does not put the two chains in one level at all.

## On real code

On the kube-rs and DataFusion benchmark branches (11 annotated functions: 6 concurrent levels and 7
prefetched loops), compiling every dependency through the driver from scratch takes 36 s
(kube-rs) and 52 s (DataFusion) on 16 cores. Both report no warnings. kube-rs gets one note:
`dynamic_api`'s `main` runs `tracing_subscriber::fmt::init()`, which may print to stderr,
concurrently with `Client::try_default()`, which may run a credential plugin process.

The first versions of the driver reported 12 false warnings on these branches. They came from
reading verbs inside libraries (`tower::Service::call`, `HeaderMap::try_append`, `tracing`'s
`set_interest`), from synchronous in-memory calls (`hashbrown::HashMap::insert`), and from treating
`Metadata::is_dir` and `File::seek` as file writes. The rules above remove them, and the fixture
test keeps the real findings.

## Installing

`psync-check` links rustc's internal crates, so it builds with a pinned nightly. Its
`rust-toolchain.toml` selects that toolchain automatically. The crate is not part of psync's
workspace, so the stable build of psync is unaffected.

```sh
cd psync-check
cargo install --path . --locked     # installs cargo-psync and psync-driver
cd /your/project
cargo psync check
```

`cargo psync` runs `cargo check` in a separate target directory (`target/psync`), with
`psync-driver` as `RUSTC_WRAPPER`. Your normal build is untouched. The whole build is compiled by
the pinned nightly, the same way `cargo clippy` compiles it with clippy's.

Like any `cargo check`, an unchanged crate is not recompiled, and cargo replays its earlier
warnings. The findings summary covers only crates recompiled in this run.

## What it does not check

- **Targets computed at run time.** `fs::write(&path)` writes "some file". Two such writes
  conflict, even when the paths differ at run time.
- **Loop items naming different objects.** The macro assumes that targets computed from the loop
  item differ between iterations. The driver does not check this yet. The roadmap's runtime target
  checks would.
- **Calls it cannot see through**: function pointers, `dyn` methods of other crates, FFI, drop glue
  (`Drop` impls), and the internals of async runtimes.
- **Verbs of other crates' APIs are still names.** `kube::Api::list` reads because its name says
  so. The driver classifies other crates' I/O by name, not by following them down to the socket:
  at that level, every HTTP request is a write.
- **Recursion.** A cycle of local functions is summarized from its members' other calls.
- **Panics** are not exits.
