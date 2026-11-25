# kube-rs and DataFusion

[Sixteen real-world shapes](./graph-suite.md) measures rewritten functions with simulated I/O.
This chapter is about whole programs, against real servers. Two projects were annotated, and
nothing else in them was changed:

- **kube-rs**, the Kubernetes client: `Discovery::run`, `Discovery::run_aggregated`,
  `ApiGroup::query_apis`, `ApiGroup::query_core`, and the `main` of the `dynamic_api` example. The
  patch is **7 lines**, including the Cargo dependencies.
- **DataFusion**, the query engine: listing-schema `refresh`, the listing-table factory's
  `create_inner`, `SessionContext::refresh_catalogs`, Arrow and Avro `infer_schema`, and the
  writer orchestration. The patch is **11 lines**.

Each application was built twice from the same tree, sequential (`PSYNC_GRAPH=off`) and psync, with
the annotated files touched before every build. The builds ran alternately.

## Production conditions

- kube-rs ran against a real Kubernetes 1.36 control plane (kube-apiserver and etcd) with 163 CRDs.
  It authenticated as a controller does, with a service-account token.
- DataFusion read from Garage, an S3 implementation, over TLS, with production-sized data.
- Network distance came from toxiproxy profiles: none (same machine), in-region (2 ms) and
  cross-region.
- There were 300 measured runs, 5 per build, scenario and profile.

| Result | Value |
|---|---|
| kube-rs discovery and inventory, in-region (geo. mean of 6 scenarios) | **4.8×** |
| kube-rs at 40 ms | 8.4× |
| DataFusion, S3 Express-like to AWS guidance latency | 4.7× to 9.3× (6.4× at S3 p90) |
| no added latency at all, 8 of 9 scenarios | faster, geo. mean 3.0× |
| slowest case: startup legacy discovery, no latency | **0.86×** (95% CI 0.79–1.06) |
| API rejections | 0 |

With no network latency there is still something to overlap: the servers' own work (TLS,
authentication, flow control, encoding, storage). The one slower case opened 23 TLS connections at
once instead of reusing one, for requests that each took under a millisecond. With pooled
connections, the same operation was 4.0× faster.

## Whole applications, start to exit

The unmodified programs that call the rewritten functions were timed from `exec` to exit:

- kube-rs's `dynamic_api` (a `kubectl get all`) and `kubectl` examples;
- `datafusion-cli`.

The time spent inside the rewritten functions was measured in the same runs. For kube-rs it came
from the API server's audit log. For DataFusion it came from the CLI's per-statement timings.

| Result | Value |
|---|---|
| best kube-rs application, end to end | **9.2×** (`dynamic_api`, remote) |
| best DataFusion application, end to end | **4.1×** |
| all applications and profiles, geometric mean | **2.59×** |
| applications where the functions are under 10% of the run | 0.89–1.21× (every interval includes 1) |
| failures | none in 320 runs; both builds printed the same output and sent the same API requests |

How much of the speedup reaches the user depends on the share of time spent in the rewritten
functions:

- Amdahl's law predicted DataFusion's end-to-end result to within 3%.
- kube-rs's `dynamic_api` got about half its prediction. Once the waits overlapped, parsing and
  printing thousands of objects ran on one core, and that became the bottleneck (see
  [Patterns and pitfalls](../guide/patterns.md#overlap-moves-the-bottleneck-to-the-cpu)).
- psync used less CPU time in every application it sped up.
- It used more memory in `dynamic_api`: 32 → 66 MB, and 118 → 262 MB on a 5× cluster.

## Heavy load

Run 3 loaded the same servers four ways:

- up to 32 clients at once;
- 5× more data;
- busy neighbours on the server;
- a CPU-saturated client node.

There were 2,770 measured client runs.

| Condition | Result |
|---|---|
| one client at a time | 3.4–5.4× |
| 8 clients: kube-rs fleet throughput | 2.0× (DataFusion 1.7×) |
| 5× / 4× more data | 4.6× kube-rs, 3.9× DataFusion |
| busy server | 5.2× |
| CPU-saturated client | 4.4× |
| 32 clients: kube-rs fleet throughput | 1.03× (467 requests answered 429, all retried, no failures) |
| 32 clients: DataFusion catalog refresh | **0.76×** per client, 0.54× throughput |

The 0.76× regression has a clear cause. DataFusion's catalog refresh nests two prefetched loops,
16 × 8, so each psync client could hold up to 113 requests in flight. 32 clients offered about
3,600 concurrent requests to one storage node behind one TLS terminator, and overloaded it. psync
creates no extra work. But it can offer the same work all at once. A process-wide in-flight budget
is the psync change these results argue for.

## What these runs say

- When the rewritten functions are most of the program and they wait on the network, most of
  their speedup reaches the user.
- When they are a small part of it, nothing visible changes. There is no penalty either.
- When the server is already saturated, overlap cannot help, and unbounded overlap can hurt.
- In every run, the two builds produced the same output.
