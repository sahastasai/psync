# Comparing builds

The best evidence that psync helps your program is the same program built both ways. psync makes
this a one-variable change.

## Two switches

| Switch | Scope | Effect |
|---|---|---|
| `#[psync::graph(off)]` | one function | compiled as written; hint attributes removed |
| `PSYNC_GRAPH=off` at build time | every annotated function in the build | the same, everywhere |

Both produce exactly the code you wrote, so a regression in a psync build can be checked against
an identical source tree with psync disabled.

## Cargo does not see `PSYNC_GRAPH`

Proc-macro output is cached with the crate that uses it. Cargo does not know that the expansion
depends on an environment variable. So if you build once with `PSYNC_GRAPH=off` and once without,
the second build may silently reuse the first expansion. Do one of these:

- **Touch the annotated files before every build:**

  ```sh
  touch src/annotated.rs && PSYNC_GRAPH=off cargo build --release && cp target/release/app app-sequential
  touch src/annotated.rs && cargo build --release && cp target/release/app app-psync
  ```

- **Use two target directories:**

  ```sh
  PSYNC_GRAPH=off CARGO_TARGET_DIR=target-off cargo build --release
  CARGO_TARGET_DIR=target-psync cargo build --release
  ```

The psync benchmark harnesses (`bench/kube-harness/run.sh`) touch every annotated file before
*each* build. This lesson came from a bad run: in its first attempt, the "sequential" binary had
reused the psync expansion.

## A benchmark recipe

This is what the kube-rs and DataFusion campaigns did:

1. **Patch the project with attributes only.** Add `psync` as a dependency and `#[psync::graph]`
   on the functions you want. The kube-rs patch was 7 lines. The DataFusion patch was 11.
2. **Build both binaries from the same tree,** as above.
3. **Alternate builds run by run** (sequential, psync, sequential, ...), so drift in the network or
   the machine hits both equally.
4. **Check that the outputs are identical.** Compare what each build prints, and if you can, what
   it sends to the server. In the kube-rs runs, the API server's audit log recorded every request
   of both builds.
5. **Check that the sequential build really is sequential.** In the kube-rs runs, a sequential
   build must have at most one request in flight at a time. The report builder asserted this.
6. **Measure under realistic latency.** psync turns waiting into overlap. With no latency there is
   less to overlap, and with a saturated server there is nothing to gain (see
   [Patterns and pitfalls](./patterns.md)).

## Where to annotate

`#[psync::graph]` works on any `async fn`, including methods, and on async blocks and async closures
inside an annotated function. Nested `fn` items need their own attribute. To annotate `main`, put
`#[psync::graph]` above `#[tokio::main]`, so psync sees the `async fn` before Tokio rewrites it:

```rust,ignore
#[psync::graph]
#[tokio::main]
async fn main() -> anyhow::Result<()> { /* ... */ }
```

That is exactly how kube-rs's `dynamic_api` example was annotated for the end-to-end runs.
