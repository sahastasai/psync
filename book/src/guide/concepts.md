# Basic concepts

This chapter introduces the six words that the rest of the book uses: **node**, **edge**,
**region**, **barrier**, **chain** and **level**. Every example comes with the report that psync
really prints for it.

## Nodes and edges

psync looks at each block of async code one statement at a time. A statement it may move is a
**node**. There are three kinds:

- a `let` binding of an awaited value, or of a cheap pure value (`let key = g * 10;`);
- an awaited expression statement (`client.flush().await;`);
- an ordinary statement with possible side effects, which psync calls an **effect**
  (`out.push(x);`, `total += n;`).

(Two words to keep apart. An *effect* node is a statement of your function, and the JSON report
marks it `"effect": true`. A *side effect*, in the [Safety](../safety/effects.md) part, is what a
call does outside the program, such as writing a file, and the report lists those under
`"effects"`. An effect node may have side effects, and so may an await.)

An **edge** `a -> b` means `b` must wait for `a`. psync adds an edge when one of these holds:

| Reason in the report | Meaning |
|---|---|
| ``reads `x` `` | `b` uses a binding that `a` defines |
| ``rebinds `x` `` / ``shadows `x` `` | the two statements bind the same name, or `b` rebinds a name `a` still reads |
| ``writes `p` `` | one writes a variable or field (`self.groups`) that the other reads or writes |
| `effect order` | both are ordinary statements, which never change order relative to each other |
| ``write file "a" (fs::write) vs read file "a" (fs::read)`` | both touch the same file, service or other outside resource, and one writes ([Side effects](../safety/effects.md)) |
| ``may exit before `op` runs`` | `a` may exit early, and `b` writes something outside the program, which sequential code would then never have done ([Side effects](../safety/effects.md#writes-are-not-speculated)) |

Here is the example from the introduction:

```rust,ignore
#[psync::graph]
async fn profile_page(api: &Api, user_id: u64) -> Result<Page, Error> {
    let user = api.user(user_id).await?;
    let posts = api.posts(user_id).await?;
    let avatar = api.avatar(&user.avatar_url).await?;
    let friends = api.friends(user_id).await?;
    Ok(Page { user, posts, avatar, friends })
}
```

```text
  region 0:
    n0: user = user()? [await]
    n1: posts = posts()? [await]
    n2: avatar = avatar()? [await]
    n3: friends = friends()? [await]
    n0 -> n2 (reads `user`)
    level 0: n0->n2 || n1 || n3
  unclassified (assumed to read; `strict` treats them as writes): user, posts, avatar, friends
```

There is one edge: `avatar` reads `user`. Everything else is free. The final `Ok(Page { .. })` is
the block's value, so it stays last.

## Chains and levels

A node with exactly one predecessor, whose predecessor has exactly one successor, joins that
predecessor's **chain**. In the example, `n0 -> n2` is a chain: `avatar` starts the moment `user`
arrives. It does not wait for `posts` or `friends`. (There is one more case, for a cheap binding
read by a request and by what uses the response; [Scheduling](../internals/schedule.md) has it.)

Chains are then grouped into **levels**. A chain's level is one past the deepest chain it waits for.
All chains in a level run at the same time, joined together. When the level is done, its bindings
are handed back in program order, and the next level starts. `n0->n2 || n1 || n3` is one level of
three concurrent chains.

Chains matter. Without them, a per-item pipeline would be held back by its slowest sibling.
Here is the `chains` test from `tests/graph.rs`:

```rust,ignore
#[graph]
async fn chains() -> (u32, u32) {
    let a1 = io(T, 1).await;
    let a2 = io(2 * T, 2).await;
    let b1 = io(2 * T, a1 + 10).await;
    let b2 = io(T, a2 + 10).await;
    (b1, b2)
}
```

`a1 -> b1` and `a2 -> b2` are two chains. Each one takes 3T. Two levels of `a1 || a2` then
`b1 || b2` would take 2T + 2T = 4T. The test checks that the function finishes in 3T.

When edges cross between chains, you get more than one level. In this example two awaits
are hidden inside other statements (an argument of `push` and the scrutinee of a `match`):

```rust,ignore
async fn hoisted(api: &Api, names: Vec<String>) -> Vec<usize> {
    let mut lens = Vec::new();
    lens.push(api.len(&names[0]).await);
    match api.kind(&names[1]).await {
        Kind::A => lens.push(1),
        Kind::B => lens.push(2),
    }
    lens
}
```

```text
  region 0:
    n0: lens = Vec :: new ()
    n1: __psync_h2 = len() [await]
    n2: lens . push (__psync_h2)
    n3: __psync_s3 = kind() [await]
    n4: match __psync_s3 { Kind
    n0 -> n2 (reads `lens`)
    n1 -> n2 (reads `__psync_h2`)
    n0 -> n4 (reads `lens`)
    n2 -> n4 (writes `lens`)
    n3 -> n4 (reads `__psync_s3`)
    level 0: n0 || n1 || n3
    level 1: n2
    level 2: n4
  unclassified (assumed to read; `strict` treats them as writes): len, kind
```

psync first moved both awaits into their own bindings (`__psync_h2`, `__psync_s3`). This step
keeps the evaluation order, and [Exposing hidden awaits](../internals/hoisting.md) explains how. The
two requests now run together in level 0. The two `push`es still happen in their original order.

## Regions and barriers

Some statements must stay exactly where they are. psync calls these **barriers**. A barrier splits
the block into separate **regions**. Nodes never move across a barrier, and each region gets its own
graph.

A statement is a barrier when:

| Barrier | Why |
|---|---|
| it acquires something it keeps: an awaited `lock`, `lock_owned`, `read`, `write`, `acquire`, `acquire_owned`, `acquire_many`, `begin`, `transaction` | what runs while the guard is held is the point |
| it awaits a deliberate delay: `sleep`, `sleep_until`, `tick`, `yield_now`, `delay_for` | its position is the point |
| it reads a clock: `now()`, `elapsed()`, `duration_since()` | it measures the waits around it |
| it is marked `#[psync::seq]` | you asked |
| it hides an exit psync cannot rewrite: `?` or `return` inside a macro, `bail!`, `ensure!`, `yield`, `break` with a value | psync cannot defer it |
| it is a `let` without an initializer, an item, a `ref` binding, or a `#[psync::fresh]` binding | it cannot be handed between scopes |
| it is the block's final value expression | it must come last |

Here is a lock:

```rust,ignore
async fn with_lock(state: &Mutex<State>, api: &Api) -> u32 {
    let a = api.one().await;
    let mut guard = state.lock().await;
    guard.count += 1;
    drop(guard);
    let b = api.two().await;
    let c = api.three().await;
    a + b + c
}
```

```text
  region 0:
    n0: guard . count += 1 ;
    n1: drop (guard) ;
    n2: b = two() [await]
    n3: c = three() [await]
    n0 -> n1 (writes `guard.count`)
    level 0: n0->n1 || n2 || n3
  barrier: line 3: `let mut guard = state . lock () . await ;`: awaits `lock`, whose position is meaningful
```

`a` is alone in the region before the lock, so there is nothing for it to overlap with. The lock
stays where it is. After it, `b` and `c` run together. (The `guard.count += 1; drop(guard);` chain
completes on its first poll, so the guard is released as early as before.)

A lock acquired *inside* a nested block, and released there, does not make the statement a
barrier:

```rust,ignore
let taken = {
    let mut guard = lock_buffer(&buffer).await;
    std::mem::take(&mut *guard)
};
```

The critical section is contained in the statement, and the lock itself keeps it exclusive. The
`critical_sections_inside_a_statement_may_move` test covers this case.

## Unique handles stay ordered

A `&mut` receiver is one handle. psync treats calls through it as writes, so they stay in order:

```rust,ignore
#[graph]
async fn twice(&mut self) -> (u32, u32) {
    let a = self.hit().await;
    let b = self.hit().await;
    (a, b)
}
```

```text
  region 0 (unchanged):
    n0: a = hit() [await]
    n1: b = hit() [await]
    n0 -> n1 (writes `self`)
    level 0: n0->n1
  unclassified (assumed to read; `strict` treats them as writes): hit
```

`(unchanged)` means psync found nothing to overlap, so it emits the region exactly as written.
Suppose psync had ever missed such an edge. Then the generated code would need two `&mut self`
borrows at once, and it would not compile. It would never race.

## Ordered calls on shared handles

Some calls take `&self` but still have an order you can observe. A channel's `send`, a stream's
`next`, an I/O `write`. psync treats these as writes to their receiver, whatever the receiver is:

```rust,ignore
async fn send_both(tx: &Sender<u32>, api: &Api) {
    let a = api.one().await;
    let b = api.two().await;
    tx.send(a).await;
    tx.send(b).await;
}
```

```text
  region 0:
    n0: a = one() [await]
    n1: b = two() [await]
    n2: send() [await]
    n3: send() [await]
    n0 -> n2 (reads `a`)
    n1 -> n3 (reads `b`)
    n2 -> n3 (writes `tx`)
    level 0: n0->n2 || n1
    level 1: n3
```

Both requests run together, and `a` is sent as soon as it arrives. `b` is still sent second. The
full list of these methods is in [Reads and writes from tokens](../internals/access.md).

## What "the same" means

After the rewrite:

- every binding has the value it would have had sequentially;
- the function returns the same value, or exits at the same `?`, `return`, `break` or `continue`
  with the same value (see [Early exits and errors](./exits.md));
- ordinary statements run in the same relative order.

What can differ is **work that the sequential code would have skipped after an exit**. That is the
speculation you opt into with the attribute, and the next chapters come back to it.
