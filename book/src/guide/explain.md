# Reading the report

psync can show you its graph in two forms. `explain` prints text at compile time. `psync-expand`
prints JSON, together with the generated code. Both come from the same `Report`.

## `explain`

```rust,ignore
#[psync::graph(explain)]
async fn profile_page(api: &Api, user_id: u64) -> Result<Page, Error> { /* ... */ }
```

The report is printed to stderr while the crate compiles. With Cargo, you see it when the
annotated crate is rebuilt:

```text
psync::graph `profile_page`:
  region 0:
    n0: user = user()? [await]
    n1: posts = posts()? [await]
    n2: avatar = avatar()? [await]
    n3: friends = friends()? [await]
    n0 -> n2 (reads `user`)
    level 0: n0->n2 || n1 || n3
  unclassified (assumed to read; `strict` treats them as writes): user, posts, avatar, friends
```

Line by line:

- **`region N`** is one run of nodes between barriers. Only regions with two or more awaits are
  analyzed and listed. `(unchanged)` means nothing overlapped, so the region is emitted as written.
- **`nK: label`** is one node. A binding shows `names = callee()`. A computed value shows its
  first characters. `?` marks a node that can exit early, and `[await]` marks one that awaits.
  A node with side effects outside the program ends with them in braces:
  `{read service "user" (fetch)}` is a read, by the call `fetch`, of the service named by the
  string `"user"`. `{write file "cfg.toml" (fs::write)}` writes that file. The rules that produce
  these are in [Side effects](../safety/effects.md).
- **`nA -> nB (reason)`**: `nB` waits for `nA`. The reasons are listed in
  [Basic concepts](./concepts.md#nodes-and-edges).
- **`unclassified (..): names`** lists the awaited calls whose names say nothing about what they
  do (`user`, not `get_user`). psync assumes they only read, so they may run concurrently. Check
  that list: a call in it that really writes is the one place a token-level analysis can be wrong.
  `#[psync::graph(strict)]` treats them as writes instead, and `cargo psync check` checks them
  against the compiled callees.
- **`level L: chain || chain`** lists chains (`n0->n2`) that run concurrently in level `L`.
- **`loop `for x in xs`: prefetch [labels], N at a time`** names a rewritten loop. A loop that was
  left alone shows the reason instead.
- **`barrier: line L: `stmt`: reason`** is an await-containing statement kept in place. Line
  numbers are only known under `psync-expand`. Inside the proc macro they show as `0`.

Hoisted values appear under generated names: `__psync_hN` for an operand, `__psync_sN` for a
scrutinee, `__psync_gN` for a guarded branch, `__psync_tN` for a block's tail.

## Worked example: a false edge

A report is most useful when it shows an edge you did not expect. Take:

```rust,ignore
#[psync::graph(explain)]
async fn report(db: &Db, cache: &Cache) -> Result<Report, Error> {
    let view = db.create_view("sales").await?;
    let totals = db.get_totals("sales").await?;
    let hits = cache.lookup("sales").await;
    Ok(Report::new(view, totals, hits))
}
```

```text
  region 0:
    n0: view = create_view()? [await] {write `db` "sales" (create_view)}
    n1: totals = get_totals()? [await] {read `db` "sales" (get_totals)}
    n2: hits = lookup() [await] {read `cache` "sales" (lookup)}
    n0 -> n1 (write `db` "sales" (create_view) vs read `db` "sales" (get_totals))
    level 0: n0->n1 || n2
```

Read it from the bottom. One level, two chains: `create_view` then `get_totals` in order, and
`lookup` beside them. Why must `get_totals` wait? The edge says: psync took `create_view` for a
write (`create` is a write verb) on the receiver `db`, with the target `"sales"`, and `get_totals`
reads the same thing.

Is that true? It depends on your `Db`. If `create_view` only builds a query plan in memory, the
edge is false, and you can say so for this one name:

```rust,ignore
#[psync::graph(explain, effects(read(create_view)))]
```

```text
  region 0:
    n0: view = create_view()? [await] {read `db` "sales" (create_view)}
    n1: totals = get_totals()? [await] {read `db` "sales" (get_totals)}
    n2: hits = lookup() [await] {read `cache` "sales" (lookup)}
    level 0: n0 || n1 || n2
```

All three requests now start together. If `create_view` really does create a view on the server,
the first report was right, and changing it would let `get_totals` read before the view exists.
Before overriding a name, check what the callee does: [`cargo psync check`](../safety/check.md)
reads its body and tells you.

## `psync-expand`

`psync-expand` runs the same transformation on any function in any file, without changing the
file:

```sh
cargo run -p psync-graph --features cli --bin psync-expand -- src/discovery.rs 115 8
```

The arguments are the file, a line inside the function, and an optional bound (default 16). It picks
the innermost function containing the line. Free functions, `impl` methods and trait methods with
default bodies all work. The output is one JSON object:

```json
{
  "function": "profile_page",
  "start_line": 1,
  "end_line": 7,
  "bound": 16,
  "changed": true,
  "explain": "  region 0:\n    n0: user = user()? [await]\n ...",
  "report": {
    "regions": [
      {
        "nodes": [
          { "label": "user = user()?", "awaits": true, "effect": false, "effects": [],
            "exits": true, "line": 2, "speculate": false },
          ...
        ],
        "edges": [[0, 2, "reads `user`"]],
        "chains": [[0, 2], [1], [3]],
        "levels": [[0, 1, 2]],
        "unclassified": [[2, "user"], [3, "posts"], [4, "avatar"], [5, "friends"]],
        "changed": true
      }
    ],
    "loops": [],
    "barriers": []
  },
  "expanded": "async fn profile_page(api: &Api, user_id: u64) -> Result<Page, Error> { ... }"
}
```

- `levels` holds indices into `chains`, and `chains` holds indices into `nodes`.
- `effect: true` marks an ordinary statement. Effects keep their order with each other.
- `loops[]` entries have `label`, `prefetched` (labels of the prefix nodes), `bound`, `changed`,
  `reason`, `line` and `end_line`.
- `expanded` is the pretty-printed generated function.

Pipe it through `jq` to get one part out:

```sh
psync-expand file.rs 42 | jq -r .explain
psync-expand file.rs 42 | jq -r .expanded
```

`psync-expand` was how the 90 audit findings were checked. It ran on each finding's function at the
pinned commit, and the script confirmed that the concurrent region or prefetched loop fell inside the
finding's source lines.
