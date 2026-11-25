# Related work: dependence and diagnosis

psync rewrites programs, but two questions it raises come from older work on measuring them. Which
waits sit on the critical path of a request? Would making one part faster make the whole faster?
This page lists the work psync's planned diagnosis features build on: the conditional critical
path (board item #168) and a dependence graph of async code from MIR (#167). For each, it says
what the work does and what psync takes from it.

## The program dependence graph (Ferrante, Ottenstein and Warren, 1987)

Jeanne Ferrante, Karl Ottenstein and Joe Warren, *The Program Dependence Graph and Its Use in
Optimization*, ACM TOPLAS 9(3), 1987.

The PDG puts two kinds of edges in one graph. **Data dependences** say a statement uses a value
another defines. **Control dependences** say a statement runs only if a branch goes a certain way.
Control dependence is computed from **post-dominators**: `B` is control dependent on branch `A` when
one of `A`'s successors always leads to `B` and another may avoid it. With both in one graph, a
transformation that reorders code only needs to respect the edges, which is what psync's scheduler
does on its token-level graph ([Dependencies and dataflow](../foundations/dataflow.md)).

**For psync.** The macro's graph has data and effect edges, and early exits stand in for control
dependence: a write after a `?` waits, or needs `speculate`. A PDG built from MIR (#167) would
give exact control dependences, including branches inside callees, and guards for statements that
only run on one path.

## AppInsight (Ravindranath et al., OSDI 2012)

Lenin Ravindranath, Jitendra Padhye, Sharad Agarwal, Ratul Mahajan, Ian Obermiller and Shahin
Shayandeh, *AppInsight: Mobile App Performance Monitoring in the Wild*, OSDI 2012.

AppInsight rewrites the binaries of mobile apps to follow a **user transaction**: from a tap,
through the asynchronous callbacks and threads it starts, to the screen update that ends it. From
the recorded edges it finds the transaction's **critical path**, the chain of waits and work that
decided how long the user waited. It does this in deployed apps, without changing the developer's
code.

**For psync.** A function rewritten by psync is a small transaction: chains joined in levels. Its
critical path is the slowest chain of each level, and only shortening that chain helps. Joining
measured span latencies to psync's graph (#168) is AppInsight's idea applied to one function,
with the graph known statically instead of recovered from callbacks.

## Panappticon (Zhang et al., CODES+ISSS 2013)

Lide Zhang, David Bild, Robert Dick, Z. Morley Mao and Peter Dinda, *Panappticon: Event-Based
Tracing to Measure Mobile Application and Platform Performance*, CODES+ISSS 2013.

Panappticon traces events in the Android framework and the kernel, such as message queues, binder
calls, locks and I/O. It links them into the transactions of an app, so a delay can be traced to
the platform and not only to the app's code.

**For psync.** Delays outside the function also count: a connection pool, a server's flow control
([Patterns and pitfalls](../guide/patterns.md)), or the executor's queue. Diagnosis from spans
alone sees them only as long waits, and Panappticon shows the value of tracing below the
application.

## Timecard (Ravindranath et al., SOSP 2013)

Lenin Ravindranath, Jitendra Padhye, Ratul Mahajan and Hari Balakrishnan, *Timecard: Controlling
User-Perceived Delays in Server-Based Mobile Applications*, SOSP 2013.

Timecard tells the server, while it handles a request, how much time the request has already
spent and how long the rest of its trip, the response's way back and its rendering, is predicted to
take. The server can then change its work, for example by returning a cheaper answer, to meet a
deadline for the whole transaction.

**For psync.** psync changes only the order of a function's work, not its amount. Timecard is a
reminder that latency belongs to the whole request, and that a rewrite which shortens one function
matters only if that function is on the request's critical path.

## Coz (Curtsinger and Berger, SOSP 2015)

Charlie Curtsinger and Emery Berger, *Coz: Finding Code that Counts with Causal Profiling*,
SOSP 2015.

A conventional profiler shows where time is spent. Coz shows **what would happen if a line were
faster**. It runs a **virtual speedup**: while the chosen line runs, every other thread is paused
for a fraction of that time, which has the same relative effect as making the line faster. It then
measures how the rate of **progress points**, such as requests completed, changes. Code that takes
a lot of time but is not on the critical path shows no effect.

**For psync.** psync's question before a rewrite is the same: if these waits overlapped, how much
faster would the function be? The schedule implies an answer: each level takes as long as its
slowest chain. psync does not compute that number yet. Today [Comparing builds](../guide/comparing.md)
measures the real answer. A what-if report that predicts from measured spans and is checked by a
measurement (#168) would be a causal profile for one kind of change, reordering waits, with the
graph saying which reorderings are allowed.

## Where psync differs

All four diagnosis tools observe a program and tell a person where to look. psync changes the
program. It proves from the graph which waits may overlap, and it checks the rewrite with the
compiler ([Checking rewrites](../safety/check.md)). The planned diagnosis features would close the
loop. They would measure which chains are on the critical path, predict the gain of a hint such as
`speculate` or `independent`, and verify the prediction by building and measuring both versions.
