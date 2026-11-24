# Introduction

*Dedicated to Baba.*

Async Rust is sequential unless you say otherwise. Every `.await` waits for the one before it to
finish, even when the two have nothing to do with each other:

```rust,ignore
async fn profile_page(api: &Api, user_id: u64) -> Result<Page, Error> {
    let user = api.user(user_id).await?;
    let posts = api.posts(user_id).await?;               // waits for `user`, but doesn't need it
    let avatar = api.avatar(&user.avatar_url).await?;
    let friends = api.friends(user_id).await?;           // waits for all three
    Ok(Page { user, posts, avatar, friends })
}
```

This function makes four round trips one after another. Only one of them, `avatar`, needs an
earlier result. The others could run at the same time. Rust gives you the tools to do that:
`join!`, `try_join!`, `FuturesUnordered`, `buffered(n)`. But you have to notice the chance, work out
the dependency graph by hand, and rewrite the function into a shape that is harder to read. In
practice this is often skipped. Even when someone does it, the result is usually not the best
schedule.

psync does that rewrite for you. Add one attribute:

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

At compile time, psync builds the function's dependency graph. It then runs `user → avatar`,
`posts` and `friends` as three concurrent chains. The function returns the same values. It also
returns the same error: if both `posts` and `friends` fail, you still get the error from `posts`,
because that is the one the sequential code would have hit first.

## What psync is

psync has two parts:

- **`#[psync::graph]`**: a procedural macro that rewrites an async function so that awaits which
  do not depend on each other run concurrently. This covers straight-line code, loops (iterations
  run ahead, up to a bound) and work behind `if` guards. The generated code uses only `core` and
  `alloc`. It runs on any executor: Tokio, smol, embassy, or psync's own.
- **`psync::Executor`**: a small `no_std`, heap-backed, cooperative executor with task
  priorities, written for the RP2350 microcontroller. It also runs on desktop and server machines.

You can use either part without the other. Most of this book is about the macro.

## Why it is safe to try

psync only reorders code in ways the Rust compiler can still check:

- The generated code is ordinary Rust, so it is type-checked and borrow-checked like the rest of
  your code. Suppose two branches both need `&mut x`. psync's analysis keeps them in order. If it
  ever failed to, the generated code would not compile. It would never race.
- Results, bindings and early exits (`?`, `return`, `break`, `continue`) behave as in the
  sequential code, and the function exits at the same place with the same value.
- Ordinary statements keep their order. So do locks, permits, transactions, sleeps, clock reads,
  and order-sensitive calls such as `send`, `write` and `recv`. Everything else that is fixed in
  place is listed in [Basic concepts](./guide/concepts.md).
- One build switch, `PSYNC_GRAPH=off`, compiles every annotated function exactly as written. This
  makes it easy to compare the two builds and to back psync out.

There is one thing you do accept when you add the attribute. Awaited operations may run
*speculatively*: when the sequential code would have stopped early, psync may already have started
work after that point. For reads this is harmless. For writes it may not be. The
[side-effects chapter](./safety/effects.md) describes how psync recognizes file-system, network and
process effects and keeps them from running speculatively.

## In numbers

| Result | Where it comes from |
|---|---|
| **9.2×** faster end to end | kube-rs's `dynamic_api` example (a `kubectl get all`), remote profile, whole process from start to exit |
| **4.1×** faster end to end | `datafusion-cli` sessions on S3-like storage |
| **2.59×** | geometric mean over all applications and network profiles, end to end |
| **0 failures in 320 runs** | both builds printed the same output and sent the same API requests |
| **7 and 11 lines** | the whole patch to kube-rs and to DataFusion: attributes, no rewrites |
| **79 of 90** | confirmed real-world findings that psync rewrites from the code as written; 10 more need one hint line |
| **3.79×** | geometric mean over the 16 real-world shapes in `graph_suite` |

The "findings" are from **the audit** that motivated psync: a search of 35 popular async Rust
repositories, at pinned commits, for awaits that ran one after another but did not need to. It found
193 verified examples, 90 of them confirmed unconditionally. Each has an id such as `web-16` (a
Lemmy handler) or `tools-13`, which the book uses to cite it. 99 of the 193 were awaits in a loop.

The end-to-end numbers come from measured runs against a real Kubernetes 1.36 control plane and an S3 store over TLS.
The [case studies](./case-studies/kube-datafusion.md) cover them, including the cases where psync
did not help or made things slower.

## Where psync is going

psync started from a research proposal with two phases:

- **Phase I, asynchronous concurrency.** In three steps: (1) extend the executor to several
  cores; (2) build the token-level dependency graph in a procedural macro; (3) refine it with
  compiler information (HIR, THIR, MIR: types, resolved calls, aliasing, effects). The macro
  described in this book is step 2, and [`cargo psync`](./safety/check.md) is step 3.
- **Phase II, synchronous concurrency.** Apply the same analyses to ordinary synchronous code.
  Split independent regions into tasks, and run them on parallel workers.

The macro sees tokens, not types. That is why it is deliberately conservative, and why some
decisions need a hint from you. [What is checked, what is assumed](./internals/soundness.md) and
[Roadmap: beyond tokens](./safety/roadmap.md) say exactly where the line is.

## How to read this book

- [Getting started](./getting-started.md) takes you from an empty project to your first concurrent
  function.
- **Guide** explains what psync does with each kind of code, how to read its report, and how to
  steer it with hints.
- **Foundations** teaches the compiler ideas the rest of the book relies on, from zero: tokens,
  syntax trees, procedural macros, rustc's HIR, THIR and MIR, dataflow, effect summaries,
  class-hierarchy analysis and proofs. Each chapter has runnable examples and exercises. Read it
  before **How psync works** if those words are new to you.
- **How psync works** walks through the implementation: classification, the token-level read/write
  analysis, the graph, the schedule and the generated code.
- **Safety** covers side effects: files, network, processes.
- **Case studies** show psync on real projects.
- **Contributing** is a tour of every crate, and recipes for adding an effect rule, a check or a
  test.
- **Reference** lists every option.
