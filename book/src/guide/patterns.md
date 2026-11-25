# Patterns and pitfalls

This chapter collects what running psync on real projects taught us: the shapes it handles well,
the ones it deliberately leaves alone, and the ways overlap can hurt.

## Shapes psync handles well

The audit of 35 popular Rust repositories sorted missed concurrency into patterns. psync covers the
async ones:

| Pattern | Example | What psync does |
|---|---|---|
| P1 independent awaits | two RPCs in a row that share nothing | one level, several chains |
| P2 await in a loop | `for key in keys { store.head(key).await? }` | prefetch, bounded |
| P3 per-item chains | per shard: list, then fetch, then read the manifest | chain fusion, so each item proceeds on its own |
| P4 late start | a download that needs only the config, written after a long chain | moves into the first level |

[Sixteen real-world shapes](../case-studies/graph-suite.md) shows one function per pattern from real
repositories, with timings.

## Things that stay in place on purpose

- **Locks, permits and transactions.** An awaited `lock`, `read`, `write`, `acquire` or
  `transaction` whose guard outlives the statement is a barrier. If the guard is dropped inside the
  statement's own block, the statement may still move. The critical section stays exclusive either
  way.
- **Sleeps and ticks.** `sleep`, `sleep_until`, `tick`, `yield_now` and `delay_for` are barriers.
  A retry backoff must wait for the failure before it.
- **Clock reads.** `now()`, `elapsed()` and `duration_since()` measure the waits around them. Before
  clock reads were made barriers, kube's `dynamic_api` example timed its discovery as 0 ms: the
  clock was read before the requests it was meant to time.
- **Ordered channel and stream calls.** `send`, `recv`, `next`, `write`, `flush`, `execute`,
  `commit`, ... keep their order per receiver.

## `&mut` in two branches does not compile

psync tracks `&mut` handles and `mut` bindings, and keeps their uses in order. It does not see
types, though, so a `&mut` returned by a method slips through:

```rust,ignore
pub struct Pool { conn: Conn }
impl Pool {
    pub fn conn(&mut self) -> &mut Conn { &mut self.conn }
}
impl Conn {
    pub async fn query(&mut self, q: u32) -> u32 { /* .. */ }
}

#[psync::graph]
pub async fn two_queries(pool: &mut Pool) -> u32 {
    let conn = pool.conn();          // a `&mut Conn`, but nothing in the tokens says so
    let a = conn.query(1).await;
    let b = conn.query(2).await;
    a + b
}
```

`query` is not a name psync knows, so it is assumed to read, and the two queries go into two
concurrent chains. Both chains now need `conn` mutably at once, and rustc rejects the generated
code:

```text
error[E0524]: two closures require unique access to `*conn` at the same time
  --> src/lib.rs:10:1
   |
10 | #[psync::graph]
   | ^^^^^^^^^^^^^^^
   | |
   | closures are constructed here in different iterations of loop
   | first borrow later captured here by closure
...
13 |     let a = conn.query(1).await;
   |             ---- first borrow occurs due to use of `*conn` in coroutine
14 |     let b = conn.query(2).await;
   |             ---- second borrow occurs due to use of `*conn` in coroutine
```

(The "different iterations of loop" is rustc describing the generated join. There is no loop in
your code. Other shapes give `E0499: cannot borrow .. as mutable more than once`. Both mean the
same thing.)

That is the intended failure mode: **a missed dependency becomes a compile error, not a race.**
Two fixes:

- **`#[psync::seq]`** on the second query keeps it in place. This always works, and here it is
  the right fix: the two queries share one connection, so they could not overlap anyway.

  ```rust,ignore
  let a = conn.query(1).await;
  #[psync::seq]
  let b = conn.query(2).await;
  ```

- **`#[psync::fresh]`** on `let conn = ..` gives each branch its own connection, by running the
  initializer once per branch. It works only when the initializer itself can run twice at once.
  Here it cannot: each branch would call `pool.conn()`, which borrows `pool` mutably, and the
  error moves to `pool` (``two closures require unique access to `*pool` ``). `fresh` is for
  initializers like `context.pool()` that hand out a new value from a shared reference, as in
  [Hints](./hints.md#psyncfresh).

In the audit, one finding could not be done at all, because it needs `&mut self` in both branches.

## `Send` futures

A function whose future must be `Send` (because it is `tokio::spawn`ed, say) stays `Send` after
the rewrite when every branch is `Send`. One trap was found in DataFusion and fixed. rustc cannot
prove `Send` for an iterator adapter, or a generic helper's future, over items like
`Box<dyn Trait + Send>`. So prefetched loops now create each item's future inline, before the
await. The `prefetched_trait_objects_stay_send` test keeps it that way.

## Nested loops multiply their bounds

Each prefetched loop has its own bound. An outer loop over tables (bound 16) whose body has an
inner loop over files (bound 8) can hold **16 × 8 = 128** requests in flight from one client.

In the heavy-load run, 32 DataFusion clients refreshing their catalogs offered about 3,600
concurrent requests to one storage node behind one TLS terminator. The node's throughput fell from
813 to 538 requests/s. The median request took 1.6 s instead of 50 ms. psync made that scenario
**0.76× per client** (0.54× fleet throughput). At 8 clients, by contrast, the DataFusion fleet
still completed 1.7× more operations per second with psync.

What to do today:

- give inner loops a smaller `bound`, or annotate only one level;
- use a client that multiplexes (HTTP/2), so extra requests in flight do not each cost a new
  connection.

A process-wide in-flight budget in psync would fix this properly. It has not been built yet.

## Overlap moves the bottleneck to the CPU

psync joins concurrent work inside **one task**. Once the waits overlap, whatever CPU work follows
them runs on one core. kube-rs's `dynamic_api` (a `kubectl get all`) spends 98–99% of its
sequential run in the loop psync rewrites. From that loop's own speedup, Amdahl's law predicts
10.4–11.5× end to end. It measured 5.0–9.2×. With every response arriving at once, parsing and printing thousands of objects
became the bottleneck, and all of it ran on one core.

The fix is to spawn each iteration onto the runtime's worker threads when its future is `Send`. That
would be a change inside psync, not in your code, and it is not built yet.

## Many new connections at once

With no network latency at all, one scenario got slower. kube-rs's legacy discovery at startup ran at
0.86×: each request took under a millisecond, and psync opened 23 TLS connections at once instead
of reusing one. Warm re-discovery, with a pooled connection, was 4.0× faster on the same setup.
Overlap is cheap when waiting dominates, and not free when connection setup does.

## Servers push back

At 32 concurrent clients, the Kubernetes API server's flow control queued psync's requests, and
rejected 467 of them with HTTP 429. kube-rs retried every one, and no run failed. Fleet throughput
was then the same in both builds (1.03×). psync creates no work. It only turns waiting into
overlap, and a saturated server has no waiting left to overlap.

## Memory

Prefetched loops buffer every item's prefix result until the rest of the body runs. kube-rs's
`dynamic_api` went from 32 to 66 MB peak RSS, and from 118 to 262 MB on the 5× cluster. Choose
`bound` with memory in mind for very long loops.
