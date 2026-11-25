# Hints

psync reads tokens, and some things you know cannot be seen in tokens. Four hint attributes tell
psync what you know. Inside a `#[psync::graph]` function, psync reads them and removes them from
its output. Outside one, they do nothing.

| Hint | Goes on | Means |
|---|---|---|
| `#[psync::seq]` | a statement | keep this statement exactly where it is |
| `#[psync::fresh]` | a `let` | each concurrent branch evaluates the initializer again, instead of sharing the value |
| `#[psync::independent]` | a `for` or `while` loop, or a statement | on a loop: iterations do not depend on each other, even where psync cannot prove it. On a statement: its outside effects commute with the others' |
| `#[psync::speculate]` | a statement, a loop, or the function (`#[psync::graph(speculate)]`) | its writes may run before an earlier statement that might exit, because doing them anyway is harmless |

## `#[psync::seq]`

Use `seq` when the order of two statements matters for a reason psync cannot see. For example, two
requests to a rate-limited API, an audit log, or a call that must happen before another for
business reasons:

```rust,ignore
#[graph]
async fn pinned() -> u32 {
    let a = io(T, 1).await;
    #[psync::seq]
    let b = io(T, 2).await;
    a + b
}
```

The marked statement is a barrier. It splits the block into regions, and nothing moves across it.
In the `seq_marker_keeps_order` test, the peak number of awaits in flight is 1.

## `#[psync::fresh]`

A shared `&mut` stops concurrency. When the value behind it can be re-created cheaply, for example a
connection checked out of a pool, `fresh` gives each branch its own copy:

```rust,ignore
#[graph]
async fn fresh_pool(context: &Context) -> Result<u32, Error> {
    #[psync::fresh]
    let pool = &mut context.pool();
    let a = read_row(pool, 1).await?;
    let b = read_row(pool, 2).await?;
    Ok(a + b)
}
```

```text
  region 0:
    n0: a = read_row()? [await] {read service `pool` (read_row)}
    n1: b = read_row()? [await] {read service `pool` (read_row)}
    level 0: n0 || n1
```

Each chain begins with its own `let pool = &mut context.pool();`. Uses of `pool` do not count as
conflicts. Without the hint, both reads would go through the same `&mut` and stay sequential. In the audit,
this hint was needed once, for a Lemmy handler (`web-16`). Four other findings needed `independent` on a
loop. Five more have needed a hint since psync started tracking side effects: `speculate` where a
loop writes behind a `?`, or `independent` (or `effects(..)`) where two calls on one resource
really commute. [Side effects](../safety/effects.md#what-the-audit-found) lists them.

Only use `fresh` when evaluating the initializer again is correct. It is wrong if the branches must
share one transaction, or must see each other's writes.

## `#[psync::independent]`

`independent` on a loop tells psync to skip its loop-carried-state checks. Use it when:

- **The state is a cache or dedupe structure.** Two iterations touching the same key would only
  repeat work:

  ```rust,ignore
  #[psync::independent]
  for k in keys {
      if !cache.contains_key(&k) {
          let v = io(T, k * 2).await;
          cache.insert(k, v);
      }
  }
  ```

- **The loop is a `while` loop.** psync never rewrites `while` loops without the hint, because it
  cannot see from tokens that the condition depends only on the statements before the first await
  (see [Loops](./loops.md#while-loops)).

- **An API's independence needs knowledge psync lacks.** For example, writes to different keys of a
  store, through a handle psync treats as shared.

`independent` does not turn off the borrow checker. If the iterations really need the same `&mut`
at once, the code still fails to compile.

On an ordinary statement, `independent` says that its effects on outside resources commute with
those of the statements around it. Two writes to different event types through one client are an
example (finding `app-19` in matrix-rust-sdk):

```rust,ignore
self.client.account().set_account_data(KeyBackupContent { enabled: true }).await?;
#[psync::independent]
#[psync::speculate]
self.client.account().set_account_data(BackupDisabledContent { disabled: false }).await?;
```

Data dependencies between the statements still hold.

## `#[psync::speculate]`

psync does not run a recognized write (a `put`, `delete`, `upload`, file write or process) before
an earlier statement that may exit, and it does not prefetch a loop's writes when the loop can end
early. `speculate` lifts that rule for one statement, one `for` loop, or (as
`#[psync::graph(speculate)]`) a whole function:

```rust,ignore
let mut published = BTreeSet::new();
#[psync::speculate] // content-addressed uploads are idempotent
for upload in uploads {
    if published.insert(upload.clone()) {
        client.put_blob(upload).await?;
    }
}
```

Use it when performing the write although sequential code would have stopped first is harmless:
idempotent uploads, cache fills, PUTs of the same value. Do not use it for payments, deletes or
messages. See [Side effects](../safety/effects.md).

## When to reach for `off`

When a function cannot be made correct with hints, leave it as written with
`#[psync::graph(off)]`, or just remove the attribute. Here is one example from the audit: a
function that borrows `&mut self` in both branches. No reordering can make that run concurrently.
