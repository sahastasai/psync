# Inside rustc: HIR, THIR, MIR

A procedural macro sees tokens and syntax. After macro expansion, rustc turns your program into
three more forms. Each is simpler to analyse than the one before, and each knows more. This
chapter shows all three on two small functions, using rustc's own printouts, and ends with how
`psync-driver` reads the last one.

To print these forms yourself you need a nightly compiler. The `-Z` flags are unstable. The
outputs below come from the toolchain `psync-check` pins:

```sh
rustc +nightly-2026-11-20 --edition 2024 --crate-type lib -Zunpretty=hir       sample.rs
rustc +nightly-2026-11-20 --edition 2024 --crate-type lib -Zunpretty=thir-tree sample.rs
rustc +nightly-2026-11-20 --edition 2024 --crate-type lib -Zunpretty=mir       sample.rs
```

Here is `sample.rs`:

```rust,ignore
use std::fs;

pub fn save(data: &str) -> std::io::Result<()> {
    let text = data.to_uppercase();
    fs::write("cache.json", text)?;
    Ok(())
}
```

## Between the AST and the HIR: expansion and resolution

After parsing, rustc first **expands macros**. It repeatedly finds a macro call or attribute,
runs it, and parses what it returns, until none are left. `#[psync::graph]` runs at this
point, as do `#[derive]`, `println!` and `#[tokio::main]`.

Then it **resolves names**. Every path is looked up, and `fs::write` becomes "the function
`write` in module `fs` of crate `std`". `Ok` becomes "variant `Ok` of enum `Result`", and `text`
becomes "the local variable bound on line 4". This is the step that answers the question the
syntax-trees chapter could not: which name is a variable and which is a constructor.

## HIR: the desugared program

The **HIR** (high-level intermediate representation) is the program after expansion and
resolution, **desugared**. Several pieces of Rust syntax are only shorthand for other syntax,
and the HIR writes the longhand. Here is `save` in HIR:

```rust,ignore
fn save(data: &'_ str) -> std::io::Result<()> {
    let text = data.to_uppercase();
    match branch(fs::write("cache.json", text)) {
        Break {  0: residual } => return from_residual(residual),
        Continue {  0: val } => val,
    };
    Ok(())
}
```

The `?` is gone. In its place is a `match` on `Try::branch`, which returns
`ControlFlow::Break(residual)` for an error and `ControlFlow::Continue(value)` for a success. A
`Break` returns from the function, after converting the error with `FromResidual`. That is
exactly what `?` means, and psync's generated code uses the same two traits through its own
`Branch` and `Rethrow` wrappers.

`async` and `.await` desugar too:

```rust,ignore
pub async fn load() -> String {
    std::fs::read_to_string("cache.json").unwrap()
}
pub async fn page() -> usize {
    let a = load().await;
    a.len()
}
```

```rust,ignore
async fn page() -> /*impl Trait*/ |mut _task_context: ResumeTy| {
    let a = match into_future(load()) {
        mut __awaitee => loop {
            match unsafe { poll(new_unchecked(&mut __awaitee), get_context(_task_context)) } {
                Ready {  0: result } => break result,
                Pending {} => { }
            }
            _task_context = (yield ());
        },
    };
    a.len()
}
```

Three names in it are compiler plumbing. `_task_context` holds the executor's `Context` (the
waker) for the current poll, and `ResumeTy` is its type as the coroutine sees it. `new_unchecked`
pins the inner future in place, which is safe here because the coroutine itself is pinned. Then
the most useful thing in the chapter for understanding async Rust:

- An `async fn` is a function that returns a **coroutine**, a closure-like value that can stop
  in the middle and be resumed later. Calling `page()` runs none of its body.
- `.await` is a **loop**. It polls the inner future. If the future is ready, the loop breaks with
  its value. If not, it **yields**: the coroutine stops and returns `Pending` to whoever polled
  it, normally an executor. When the executor polls again, execution resumes right after the
  `yield`, with a new task context.

So "sequential awaits" are literally sequential. The second `loop` cannot start until the first
`break`s. psync's join does what this loop does, but for several futures at once: it polls each
chain in turn and yields only when all of them are pending.

## THIR: every expression typed

Next, **type checking** runs on the HIR. It infers the type of every expression, resolves each
method call to the function it calls, and inserts the implicit operations you never write:
auto-referencing, auto-dereferencing, and coercions. The **THIR** (typed HIR) writes all of that
down. It is built one function body at a time, used to check patterns and to build MIR, and then
thrown away.

Here is the THIR for `data.to_uppercase()`, much shortened (the real printout of this
seven-line function is 563 lines):

```text
Call {
    ty: FnDef(alloc::str::{impl#5}::to_uppercase)
    fn_span: sample.rs:4:21: 4:35
    args: [
        Expr {
            ty: &str
            kind: Borrow (
                borrow_kind: Shared
                arg: Expr {
                    ty: str
                    kind: Deref { Expr { ty: &str, kind: VarRef { id: LocalVarId(..) } } }
                }
            )
        }
    ]
}
```

Three things are new:

- **Every node has a `ty`.**
- **The method is resolved.** `.to_uppercase()` is a call of one specific function, the method
  `to_uppercase` in one particular `impl str` block of `alloc` (rustc numbers a module's impl
  blocks from 0, and `{impl#5}` is the one numbered 5). The receiver has become
  the first argument.
- **Nothing is implicit.** `data` was auto-dereferenced (`Deref`) and borrowed again (`Borrow`)
  to make the `&self` argument. In the source these steps were invisible.

This is the information the macro lacks. If `api.user(id)` is a method of a trait
implemented for `Arc<Client>`, the THIR says so.

## MIR: a control-flow graph

The **MIR** (mid-level IR) is what the borrow checker, the optimizer and psync's driver work on.
It has no nested expressions at all. Each function is a **control-flow graph** (CFG):

- **Basic blocks** (`bb0`, `bb1`, ...) are runs of simple statements with one entry and one exit.
- Each block ends in a **terminator** that says where control goes next: `goto`, `switchInt`
  (a branch on an integer), `return`, `Call` (a call, then continue in a named block), `drop`,
  or `assert`.
- **Locals** (`_0`, `_1`, ...) hold every value. `_0` is the return value, `_1..` are the arguments,
  and the rest are variables and temporaries. `debug text => _2` records which local was called
  `text` in the source.

Here is `save` in MIR, complete:

```text
fn save(_1: &str) -> Result<(), std::io::Error> {
    debug data => _1;
    let mut _0: std::result::Result<(), std::io::Error>;
    let _2: std::string::String;
    let mut _3: std::ops::ControlFlow<std::result::Result<!, std::io::Error>>;
    let mut _4: std::result::Result<(), std::io::Error>;
    let mut _5: isize;
    scope 1 {
        debug text => _2;
        let _6: std::result::Result<!, std::io::Error>;
        scope 2 {
            debug residual => _6;
        }
        scope 3 {
            debug val => const ();
        }
    }

    bb0: {
        _2 = str::<impl str>::to_uppercase(copy _1) -> [return: bb1, unwind continue];
    }

    bb1: {
        _4 = std::fs::write::<&str, String>(const "cache.json", copy _2) -> [return: bb2, unwind continue];
    }

    bb2: {
        _3 = <Result<(), std::io::Error> as Try>::branch(move _4) -> [return: bb3, unwind continue];
    }

    bb3: {
        _5 = discriminant(_3);
        switchInt(move _5) -> [0: bb5, 1: bb6, otherwise: bb4];
    }

    bb4: {
        unreachable;
    }

    bb5: {
        _0 = Result::<(), std::io::Error>::Ok(const ());
        goto -> bb7;
    }

    bb6: {
        _6 = move ((_3 as Break).0: std::result::Result<!, std::io::Error>);
        _0 = <Result<(), std::io::Error> as FromResidual<Result<!, std::io::Error>>>::from_residual(copy _6) -> [return: bb7, unwind continue];
    }

    bb7: {
        return;
    }
}
```

Read it as a flowchart. `bb0` calls `to_uppercase` and continues in `bb1`, which calls
`fs::write`. `bb2` calls `branch` (the `?`), and `bb3` switches on the result's variant. `bb5` is the
`Continue` path, which builds `Ok(())`. `bb6` is the `Break` path, which converts the error. Both
reach `bb7`, which returns `_0`. `unwind continue` says what happens if the call panics: the
panic continues to unwind into the caller.

Some vocabulary:

- A **place** is a location that can be read or written: a local (`_3`), or a local with
  **projections**, such as a field (`.0`), a downcast to a variant (`as Break`), a dereference
  (`*_1`), or an index (`[_8]`). `((_3 as Break).0)` is "field 0 of `_3` viewed as `Break`".
- An **operand** is how a value is passed: `copy` a place, `move` it, or a `const`. (`copy _2`
  for a `String` looks wrong. It appears because this printout is after some optimizations, which
  may replace a `move` from a local that is never used again with a `copy`.)
- A **constant** such as `"cache.json"` is stored in an allocation, printed at the end of the dump
  (`alloc1 (size: 10, align: 1) { 63 61 63 68 .. │ cache.json }`).

Every call is a `Call` terminator with its callee resolved to a path and **generic arguments**:
`std::fs::write::<&str, String>`. To find out what a function does to the outside world, you
visit its basic blocks and look at the calls. That is all psync's driver does.

### Loops are cycles

Here is a `while` loop, for contrast:

```rust,ignore
pub fn sum(v: &[u32]) -> u32 {
    let mut s = 0;
    let mut i = 0;
    while i < v.len() { s += v[i]; i += 1; }
    s
}
```

```text
    bb0: { _2 = const 0_u32; _3 = const 0_usize; goto -> bb1; }
    bb1: { _5 = copy _3; _6 = PtrMetadata(copy _1); _4 = Lt(move _5, move _6);
           switchInt(move _4) -> [0: bb6, otherwise: bb2]; }
    bb2: { ... assert(move _10, "index out of bounds: ...") -> [success: bb3, ...]; }
    bb3: { _7 = copy (*_1)[_8]; _11 = AddWithOverflow(copy _2, copy _7);
           assert(!move (_11.1: bool), "attempt to compute `{} + {}`, which would overflow", ..) -> ...; }
    bb4: { _2 = move (_11.0: u32); _12 = AddWithOverflow(copy _3, const 1_usize); ... }
    bb5: { _3 = move (_12.0: usize); goto -> bb1; }
    bb6: { _0 = copy _2; return; }
```

(Blocks are joined onto fewer lines here.) `bb5` jumps back to `bb1`. That **back edge** makes
the graph cyclic, and the loop is that cycle. The bounds check on `v[i]` and the overflow
check on `+=` are explicit `assert` terminators, each a possible panic. Analyses over a cyclic
graph need the fixed-point method of the [next chapter](./dataflow.md).

### `async fn` in MIR

The MIR of `load` shows the coroutine. The function itself only builds it:

```text
fn load() -> {async fn body of load()} {
    let mut _0: {async fn body of load()};
    bb0: { _0 = {coroutine@asyncs.rs:1:31: 3:2 (#0)}; return; }
}
```

The body is a separate MIR function, `load::{closure#0}`. It takes the pinned coroutine and a
`Context`, and returns `Poll<String>`. Its blocks contain the actual calls:

```text
fn load::{closure#0}(_1: Pin<&mut {async fn body of load()}>, _2: &mut Context<'_>) -> Poll<String> {
    ...
        _3 = std::fs::read_to_string::<&str>(const "cache.json") -> [return: bb1, unwind: bb3];
    ...
        _4 = Result::<String, std::io::Error>::unwrap(move _3) -> [return: bb2, unwind: bb3];
```

In `page::{closure#0}`, the `load()` future that is alive across the `.await` is stored as a
*field of the coroutine* (`field _s0`). The states the coroutine can be suspended in are its
*variants* (`variant#3`). This is the state machine that `async` compiles to.

For psync this has a practical consequence. To summarize an `async fn`, the driver must look
inside the coroutine it builds. In MIR, building a closure or coroutine is the statement
`_0 = {coroutine@..}`, an `Rvalue::Aggregate` of kind `Coroutine(def_id, ..)`. The driver
follows that `def_id` (the coroutine's name inside the compiler, explained in the next section)
into the body.

## How `psync-driver` reads MIR

rustc is organized around **queries**. Instead of running passes in a fixed order, it asks a
central context, `TyCtxt` (usually named `tcx`), for what it needs: `tcx.type_of(def_id)`,
`tcx.optimized_mir(def_id)`. Each query computes its answer on first use and caches it. A
**`DefId`** names any definition: a function, closure, trait, impl or constant. It is a pair of a
crate number and an index within that crate. A **`DefPathHash`** is a hash of the definition's
full path that stays the same between compilations, which is why psync uses it as the key in
summary files.

A custom driver is rustc with **callbacks**. `psync-driver.rs` implements
`rustc_driver::Callbacks::after_analysis`. That method runs once type and borrow checking
have succeeded, and it receives `tcx`. Cargo runs the driver in place of rustc for every crate,
because `cargo psync` sets it as `RUSTC_WRAPPER`.

The core loop of `Analyzer::summarize` in `psync-check/src/driver/analysis.rs` is the MIR you
just read, as code:

```rust,ignore
let body = tcx.optimized_mir(def_id);
for data in body.basic_blocks.iter() {
    for stmt in &data.statements {
        // `_0 = {coroutine@..}` or `{closure@..}`: summarize the nested body too.
        if let StatementKind::Assign(assign) = &stmt.kind
            && let Rvalue::Aggregate(kind, _) = &assign.1
            && let AggregateKind::Closure(nested, _)
                 | AggregateKind::Coroutine(nested, _)
                 | AggregateKind::CoroutineClosure(nested, _) = **kind
        {
            let inner = self.summarize(nested);
            summary.absorb(&inner, stmt.source_info.span, None);
        }
    }
    // `_4 = std::fs::write::<&str, String>(const "cache.json", ..) -> ..`
    if let TerminatorKind::Call { func, args, .. } = &data.terminator().kind {
        match func.const_fn_def() {
            Some((callee, generic_args)) => self.call(&mut summary, .., callee, generic_args, &args, span),
            None => summary.opaque.push("call through a function pointer".into()),
        }
    }
}
```

- `func.const_fn_def()` gives the callee's `DefId` and generic arguments when the call is
  direct. If `func` is a local holding a function pointer, there is nothing to resolve
  statically, and the summary is marked **opaque**.
- `self.call(..)` decides what the call does. Calls into `std` or tokio are recognized by path:
  `std::fs::write` writes a file. If the first argument is a string constant, as
  `const "cache.json"` is, that constant is the target. Calls to the user's own functions are
  followed into their bodies. The [effects chapter](./effects.md) covers the rules.
- Each `Span` (`stmt.source_info.span`, the terminator's `fn_span`) points back at the source
  text. `span.from_expansion()` tells whether a span came from a macro, which is how the driver
  separates the user's calls from the code psync generated around them.

## Which form, for which question

| Question | Earliest form that answers it |
|---|---|
| Which statements bind which names? Where are the awaits? | syntax tree (macro) |
| Is `Ok` a variable? Which `fs` is meant? | HIR (after resolution) |
| What type is `api`? Which function does `.user(id)` call? | THIR (after type checking) |
| What does that function do, through every callee? | MIR of the whole build (driver) |
| Do two `&mut` uses overlap? | MIR (the borrow checker) |

psync's macro answers the first row and *guesses* the rest. The driver answers rows three and
four exactly, and the borrow checker answers row five on the code psync generated.

## Exercises

1. Print the HIR of a `for x in v { .. }` loop. What does it desugar to? Which method is called
   on each iteration?
2. In `save`'s MIR, which basic blocks run when `fs::write` returns `Ok`? When it returns `Err`?
3. In `sum`'s MIR, list every terminator that can panic. Which source expression causes each?
4. Why does the driver look at `stmt.source_info.span` for closures, but at the call's `fn_span`
   when it builds facts for the macro? (Hint: what span does the macro compute for a call?)
5. Run `-Zunpretty=mir` on an `async fn` with two `.await`s. How many variants does the
   coroutine have, and what does each one represent?

<details>
<summary>Answers</summary>

1. `match IntoIterator::into_iter(v) { mut iter => loop { match Iterator::next(&mut iter) {
   None => break, Some { 0: x } => { .. } } } }`. On each iteration, `Iterator::next` is called.
   psync's loop rewriting replaces this iteration with its own runtime helper (`over_at`, see
   [Loop rewriting](../internals/loops.md)), which keeps a bounded number of items' awaited
   prefixes in flight.
2. `Ok`: `bb0 → bb1 → bb2 → bb3 → bb5 → bb7`. `Err`: `bb0 → bb1 → bb2 → bb3 → bb6 → bb7`.
   (`bb4` is unreachable: a `ControlFlow` has only two variants.)
3. The `assert` in `bb2` (the bounds check of `v[i]`), the `assert` in `bb3` (overflow of
   `s += v[i]`) and the `assert` in `bb4` (overflow of `i += 1`). In a release build the overflow
   checks disappear. Note that the driver counts only *explicit* panics in the user's code
   (`unwrap`, `expect`, `panic!`) as exits. Bounds and overflow checks are everywhere, and
   counting them would make every statement an exit.
4. A fact is keyed by where the *callee* starts in the source, which the macro can compute from
   its tokens. For a method call `x.foo(a)`, `fn_span` covers `foo(a)`, and the macro uses the
   span of the method name `foo`. The terminator's own span covers the whole expression and
   would start at the receiver `x`, a position the macro would not use. For a path call
   `fs::write(..)` the two spans start at the same place, at `fs`, which is also where the macro
   starts (the span of the path `c.func`).
5. Every coroutine has three reserved states: *unresumed* (created, not yet polled), *returned*
   and *panicked*. On top of those it has one *suspended* state per `.await`, which holds the
   locals alive across that await. Two awaits give 5 variants. For
   `let a = one().await; let b = one().await; a + b` rustc prints the layout:

   ```text
   Unresumed(0): [], Returned (1): [], Panicked (2): [],
   Suspend0 (3): [_s1],          the first `one()` future
   Suspend1 (4): [_s0, _s2],     `a`, and the second `one()` future
   ```

   `a` must be stored in the coroutine because it is alive across the second await.

</details>
