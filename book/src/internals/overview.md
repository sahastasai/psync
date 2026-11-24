# The pipeline

The rest of the book is about what psync does. This part is about how. The code lives in two
crates:

| Crate | Role |
|---|---|
| `psync-macros` | the proc-macro entry points (`graph`, `seq`, `fresh`, `independent`); `graph` calls `psync_graph::expand` |
| `psync-graph` | the analysis and code generation, an ordinary library (also used by `psync-expand`) |
| `psync` (`src/graph_rt.rs`, exported as `psync::__private`) | the small runtime the generated code calls |

The proc macro is a thin wrapper, so the whole analysis can be tested and run outside the compiler.
`psync-expand` does exactly that.

## From attribute to code

`psync_graph::expand(attr, item)`:

1. Parses the options (`bound`, `explain`, `off`, `crate`). If `off` is set, or `PSYNC_GRAPH=off`,
   it only strips hint attributes and returns the function.
2. Builds a **`Scope`** of mutability facts for the whole function. These are the `mut` bindings,
   the bindings that hold `&mut` values (including `&mut self` and `&mut T` parameters), and the
   `#[psync::fresh]` bindings with their initializers.
3. Runs the **`Rewriter`** (`rewrite.rs`) over the body. It is a `syn::visit_mut::VisitMut` that
   handles blocks innermost first. It only rewrites code inside `async fn`s, async blocks and async
   closures, and it skips nested items.
4. Strips the hints, and prints the report if `explain` was given.

For each block of async code, the rewriter does five steps:

```text
 statements
    │  for/while loops ──► loops.rs: prefetch, guards, fission, flattening
    ▼
 hoist_tail          an awaited block value becomes `let t = ..; t`
    ▼
 expose_awaits       awaits in scrutinees, arguments, receivers and if-branches become bindings
    ▼
 schedule_block      classify each statement; split into regions at barriers
    ▼
 schedule_region     graph → chains → levels → generated joins
```

A block whose regions all stay sequential is emitted **exactly as written**. Hoisting is undone
too. So psync never changes code it cannot speed up.

## The modules

| Module | What it decides |
|---|---|
| `classify.rs` | node or barrier; awaits, exits, effects; labels |
| `access.rs` | which variables and field paths a statement reads and writes |
| `hoist.rs` | how to split a statement so its await becomes its own node |
| `schedule.rs` | edges, chain fusion, levels, and the generated join |
| `exits.rs` | how `?`/`return`/`break`/`continue` are recorded and replayed |
| `loops.rs` | loop prefetch, guard hoisting and inversion, fission, nested-loop flattening |
| `rewrite.rs` | the walk that ties it together |

The next chapters take them one at a time.
