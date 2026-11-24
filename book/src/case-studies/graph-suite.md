# Sixteen real-world shapes

`examples/graph_suite` rebuilds 16 functions from popular Rust repositories. Each one is taken from
the concurrency audit and named with its repository, file and finding id. Every I/O call is replaced
by a timer with realistic latency. For each function, the suite:

1. prints the original code and times it;
2. prints the code `#[psync::graph]` generates and times it;
3. asserts that both versions return the same result.

Both versions run on psync's executor.

```sh
cargo run --example graph_suite --no-default-features             # everything
cargo run --example graph_suite --no-default-features -- --brief  # timings only
cargo run --example graph_suite --no-default-features -- kube     # matching examples
```

## Results

From one run (`--brief`):

```text
pattern                                           initial    rewrite  speedup  origin
P1 independent awaits                            205.9 ms    60.2 ms    3.42x  anza-xyz/agave
P1 independent awaits                            271.6 ms    60.4 ms    4.50x  matrix-org/matrix-rust-sdk
P1 independent awaits                             55.6 ms    35.5 ms    1.57x  loco-rs/loco
P1 independent awaits                             48.7 ms     8.2 ms    5.94x  LemmyNet/lemmy
P2 await in a loop                               242.0 ms    40.5 ms    5.98x  delta-io/delta-rs
P2 await in a loop                               301.9 ms    50.3 ms    6.01x  apache/datafusion
P2 await in a loop                               225.7 ms    45.2 ms    5.00x  LemmyNet/lemmy
P2 await in a loop (fission)                     211.4 ms    60.3 ms    3.50x  jdx/mise
P2 await in a loop (while-loop fission, hinted)   270.7 ms    45.1 ms    6.00x  launchbadge/sqlx
P2 await in a loop (fission)                     161.0 ms    40.5 ms    3.97x  apache/iggy
P3 per-item chains                               266.7 ms    45.3 ms    5.88x  kube-rs/kube
P3 per-item chains                               605.0 ms   191.9 ms    3.15x  neondatabase/neon
P3 per-item chains                               347.3 ms    50.5 ms    6.88x  quickwit-oss/quickwit
P4 late start                                    110.3 ms    70.3 ms    1.57x  GreptimeTeam/greptimedb
P4 late start                                    200.5 ms   120.5 ms    1.66x  sigp/lighthouse
P4 late start                                    175.8 ms    70.4 ms    2.50x  zed-industries/zed
geometric mean speedup over 16 examples: 3.79x
```

Four examples carry `speculate` hints since the [effect analysis](../safety/effects.md) landed:
matrix-rust-sdk's eight account-data PUTs, mise's content-addressed uploads, iggy's per-table
writes, and GreptimeDB's SST uploads. Without them, psync keeps those writes behind the `?`s before
them, and the four run at about 1.0×. GreptimeDB also dropped from 1.83× to 1.57×. Its two uploads
for the same `file_id` now keep their order, because tokens cannot show that they write different
files.

Timings come from simulated latency, so they show the schedule psync builds, not network
behaviour. For real networks, see [kube-rs and DataFusion](./kube-datafusion.md).

## What each pattern shows

- **P1, independent awaits.** These are straight-line regions with one level. The speedup is the
  sum of the waits over the longest one.
- **P2, await in a loop.** Loop prefetch is bounded by `bound`. Three of these need fission:
  - mise dedupes blob digests before uploading;
  - iggy takes each table's batch out of a routing map before writing;
  - sqlx reserves connection slots in a `while` loop before connecting, which needs
    `#[psync::independent]`.
- **P3, per-item chains.** Each item's requests depend on each other, but items do not.
  - kube's `Discovery::run` lists API groups, then queries each queryable group.
  - neon's tenant scan runs list, then index parts, then the manifest, for each shard.
  - quickwit's delete planner is an early-`continue` guard between two awaits per split. Guard
    inversion lets the second await run ahead too.
- **P4, late start.** A request that needs little is written after a long chain. psync starts it in
  the first level:
  - GreptimeDB's upload does not need the index;
  - Lighthouse's genesis download needs only the network config;
  - Zed's database read needs nothing from S3.

## One shape in full

Here is kube's discovery loop, reduced to its dependency structure. It is the
`guarded_loop_bodies_prefetch_with_field_sensitivity` test:

```rust,ignore
impl Discovery {
    #[graph(bound = 8)]
    async fn run(mut self, all: Vec<u32>) -> Result<Self, Error> {
        self.groups.clear();
        for g in all {
            let key = g * 10;
            if self.mode.is_queryable(g) {
                let versions = io(T, if g == 99 { Err(Error("bad group")) } else { Ok(self.client + g) }).await?;
                self.groups.push((key, versions));
            }
        }
        Ok(self)
    }
}
```

Two features make this loop concurrent:

- The guard: queries run ahead only for queryable groups.
- Field sensitivity: `self.groups.push` writes `self.groups`, and the prefix reads `self.mode` and
  `self.client`. Those do not overlap.

An error on group 99 is still returned as the first failing group, in order.
