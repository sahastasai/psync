# The graph and the schedule

A region is a run of nodes between barriers. Regions with fewer than two awaits are left alone.
`schedule::plan(nodes)` turns each remaining region into a schedule.

## Edges

For every pair `i < j`, `depends(nodes[i], nodes[j])` returns the reason `j` must wait for `i`, if
there is one. The rules are listed in [Reads and writes from tokens](./access.md#edges). Edges always
point forward in program order, so the graph is acyclic by construction.

## Chain fusion

Nodes are visited in program order. Node `v` joins the chain of its predecessor `u` when:

- `u` is `v`'s last predecessor, and all of `v`'s predecessors are in `u`'s chain (members of a
  chain run in order, so when `u` has run, they all have);
- `u` is currently the last node of its chain; and
- either `u` has **only one** successor (`v`), or `u` does not await and every other successor of
  `u` also (transitively) waits for `v`.

The second case covers a cheap binding that two later nodes read:

```rust,ignore
let key = format!("{prefix}/{id}");
let resp = store.get(&key).await?;   // reads key
cache.insert(key, resp);             // reads key, and waits for resp anyway
```

Fusing `key` into `resp`'s chain delays nothing, and it keeps the per-item pipeline whole.

The first condition allows several edges into `v` as long as they all come from one chain. In
tauri-bundler, the second download reads both `linuxdeploy_arch` and `linuxdeploy`, which is
computed from it:

```rust,ignore
let linuxdeploy_arch = if arch == "i686" { "i386" } else { arch };
let linuxdeploy = tools_path.join(format!("linuxdeploy-{linuxdeploy_arch}.AppImage"));
#[psync::par]
if !linuxdeploy.exists() { /* download */ }   // reads both
```

The edge from `linuxdeploy_arch` is implied by the other two. Requiring a single predecessor left
the download in a chain of its own, a level after the first download.

Any other node starts a new chain.

## Levels

A chain's level is one more than the highest level of any chain it waits for. Chains on the same
level run concurrently. Levels run one after another.

Exits add one constraint. A chain's exits can be replayed only after every statement before them
in program order has run, so `plan` gives each chain a **release** level: its own level, or the
highest level of any chain holding an earlier statement, if a chain with an exit runs ahead of one.
Its exits are replayed, and its bindings handed back, after the release level. A chain that waits for it
gets a level after its release. Levels and releases are computed together until neither changes.

```text
level 0: n0->n2 || n1 || n3
```

Levels are a deliberate simplification. A level waits for **all** of its chains before the next
level starts. So when edges cross chains, a later chain may wait longer than its own inputs need.
Chain fusion removes the most common case of this, independent per-item pipelines. A general
dataflow schedule, where every node starts the moment its own inputs are ready, would need a
different kind of join. psync does not do this yet.

## Emitting a level

A level with one chain is emitted as the original statements. A level with several chains becomes
one block:

```rust,ignore
{
    let (__psync_taken0, __psync_taken1) = {
        let mut __psync_f0 = ::psync::__private::maybe_done_at::<1, 0, 2, true, 0, false, false, _>(async { /* chain 0 */ });
        let mut __psync_f1 = ::psync::__private::maybe_done_at::<1, 1, 1, true, 1, false, false, _>(async { /* chain 1 */ });
        ::core::future::poll_fn(|__psync_cx| {
            let __psync_done0 = __psync_f0.poll_step(__psync_cx);
            let __psync_done1 = __psync_f1.poll_step(__psync_cx);
            if true && __psync_done0 && __psync_done1 { Poll::Ready(()) } else { Poll::Pending }
        })
        .await;
        (__psync_f0.take(), __psync_f1.take())
    };
    let (/* exit slots */, __psync_out0) = __psync_taken0;
    let (/* exit slots */, __psync_out1) = __psync_taken1;
    /* re-raise recorded exits, sorted by source position */
    let (a, b) = __psync_out0.unwrap();
    let (c,) = __psync_out1.unwrap();
}
```

Details that matter:

- **The futures live in an inner block.** They, and whatever they borrow, are dropped as soon as
  their outputs are taken. So later statements may move values the branches had only borrowed. A
  bug found while compiling kube-rs forced this.
- **Each chain hands back only what later code needs.** psync scans every later statement outside
  the chain. The last binding of each name wins, as shadowing does.
- **`mut` is dropped inside a chain** for bindings the chain does not write. The handed-back
  binding keeps its `mut`. This avoids `unused_mut` warnings without changing meaning.
- **Exits are re-raised in program order**, sorted by each node's position in the block, after the
  chain's release level (see [Levels](#levels)). That makes the first exit sequential code would
  hit the one that happens. Chains are numbered across the whole region (`__psync_out3`), so a
  chain whose release comes later is not shadowed by a later level's chain.
- The whole block is emitted in place of the region's statements, and the statements after it see
  the same bindings as before.
