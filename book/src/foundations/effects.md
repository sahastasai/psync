# Effects and summaries

The dependence analysis of the [last chapter](./dataflow.md) compares what statements read and
write *in the program's own variables*. But two statements with no variables in common can still
interfere:

```rust,ignore
warm().await?;               // writes cache.json
let s = load().await?;       // reads cache.json
```

Nothing connects these two lines in the source. The connection is outside the program, in the
file system. Anything a function does that is visible outside its own variables is a **side
effect**, or just an **effect**: reading or writing a file, sending a request, starting a
process, setting an environment variable, printing. A function with no effects is **pure**. You
can run two pure functions in either order, or at the same time, and nothing outside can tell.

This chapter is about knowing which effects a call has. That needs **interprocedural
analysis**, an analysis that looks past one function into the functions it calls.

## Describing an effect

psync describes an effect by three things (`Effect` in `psync-check/src/lib.rs`):

| Part | Values | Example |
|---|---|---|
| **domain**: where it lands | file system, sockets, environment, a child process, standard output, or a named service (another crate's API) | `Fs`, `Service("kube::Api::<K>")` |
| **access**: how | `Read`, `Write`, or `Ordered` (a write whose order is visible, like `send` or `append`) | `Write` |
| **target**: what exactly, when known | a file path or key, taken from a string constant | `"cache.json"` |

Two effects **conflict**, and must keep their order, when all of these hold (`psync_check::conflict`):

1. at least one of them writes;
2. they are in the same domain. A child process may touch anything, so it conflicts with
   everything except a *read* of the environment;
3. their targets may be the same. Unknown targets may be anything. File paths overlap when they
   are equal or one is a directory containing the other. Two ordered writes on one domain
   always conflict, whatever their targets.

The two lines above conflict: one writes the file `cache.json` and the other reads it. Two reads
never conflict. Writes to `a.txt` and `b.txt` do not conflict. A write to `logs/` and a read of
`logs/today.txt` do.

## Effects and exits: speculation

Effects interact with early exits as well as with each other. In

```rust,ignore
s.check().await?;
let t = s.get_token().await;     // writes the file "token"
```

sequential code never calls `get_token` if `check` fails. If psync started both calls at once,
the file would be written even when `check` fails. That is a **speculative write**: work done
before knowing it was needed, which cannot be undone. Running a read speculatively is harmless,
because a read changes nothing. So psync adds one more rule: a write never starts before an earlier
statement that may exit, unless you mark it `#[psync::speculate]`. A call that may *panic*
counts as an exit too. The theorem `speculation_harmless` in the [proofs chapter](./proofs.md) is
the formal half of the reason reads are safe to run past an exit: extra reads change no state.
The other half, that exits are then replayed exactly as sequential code takes them, is enforced by
the generated code and not yet modeled in Lean.

## Two ways to know what a call does

**By name.** The macro sees only `s.get_token()`. It splits the name into words and looks for a
verb: `get`, `list` and `read` read, `put`, `delete` and `create` write, `send` and `append` are
ordered. That is `effects.rs`, and the table of verbs is in
[Side effects](../safety/effects.md). Names are a good guess and nothing more. `get_token`, it
turns out, writes a file.

**By body.** The driver sees the MIR of `get_token`. Its body calls `std::fs::write` with the
constant `"token"`, so it writes the file `token`. That is what the function really does. It holds
whatever the function is called.

The second is better whenever it is available, and the rest of this chapter is about computing it.

## Summaries

The **summary** of a function is the set of effects a call to it may have, through every
function it calls. The effects of a single basic block are easy to find. The hard part is
calls, because a function's summary includes its callees' summaries:

```text
S(f) = own(f) ∪ S(g₁) ∪ S(g₂) ∪ ...     for every function gᵢ that f calls
```

Here `own(f)` holds the effects of the calls `f` makes that psync recognizes directly, such as
`std::fs::write`, `tokio::fs::read` or `Command::spawn`. The driver looks those up by path. It
never follows calls into `std` or tokio. Their effects are what their paths say.

If the **call graph** (one node per function, an edge `f → g` when `f` calls `g`) has no cycles,
the equation can be solved bottom-up: summarize callees first and remember each result. This is
**memoization**. Each function is summarized once, however many callers it has.

### Recursion: cycles in the call graph

Recursive functions make the call graph cyclic, and then "callees first" has no meaning. Run
the example:

```sh
cargo run -p psync-graph --example summaries
```

```text
memoized, cut cycles:
  walk      {"read net", "write file walk.log"}
  visit     {"read net"}
  ...
least fixed point:
  walk      {"read net", "write file walk.log"}
  visit     {"read net", "write file walk.log"}
```

`walk` writes `walk.log` and calls `visit`, and `visit` calls `walk`. The first method does a
depth-first walk with memoization, and it cuts a cycle by treating a function it is already in the
middle of as having no effects. Summarizing `walk` first, it computes `visit` while `walk` is
still unfinished, so `visit` gets no write. That wrong answer is then *memoized*, and every later
question about `visit` gets it. A call to `visit` really does write `walk.log`, through `walk`.

The equation is the same kind as liveness in the [dataflow chapter](./dataflow.md): a set
defined in terms of itself. The cure is the same too. Start each summary at `own(f)`, and keep
absorbing callees' summaries until nothing grows. Sets only grow, and there are finitely many
effects, so this stops at the **least fixed point**, which gives each function exactly the effects
it can reach.

The driver does this efficiently in two steps (`Analyzer::summarize` and `component` in
`analysis.rs`):

1. **Strongly connected components.** A *strongly connected component* (SCC) is a maximal set of
   functions that can all reach each other. Here `{walk, visit}` is one, and every non-recursive
   function is a component on its own. **Tarjan's algorithm** finds the components in one
   depth-first walk, and it completes each component only after all the components it calls. So
   when a component is complete, everything outside it that it calls is already summarized.
2. **A fixed point per component.** Start each member at its own effects, and absorb its callees'
   summaries, from the memo for those outside the component and from the current round for those
   inside, until no member's summary grows. A component without a cycle takes one round.

An earlier version of the driver used the cut-cycle method. The fixture
`psync-check/tests/fixtures/hidden` now contains this exact pair (`walk`, `visit`), with `walk`
summarized first. The test checks that `cargo psync check` reports a call of `visit` as writing
`walk.log`:

```text
warning: psync runs this concurrently with other work that has a conflicting effect: this does
         `read file "walk.log"` (via read_log -> std::fs::read), the other does
         `write file "walk.log"` (via visit -> walk -> std::fs::write)
```

The `via` chain is the path through the call graph that the effect came along.

## Across crates

A crate is compiled before the crates that depend on it, and the crate graph has no cycles. So
the driver writes each crate's summaries to a file, `target/psync/summaries/<crate>-<id>.json`.
When it compiles a dependent crate, it reads those files and looks callees up by their
`DefPathHash`. The crate graph is a DAG, so no fixed point is needed across crates. Cargo's build
order is already "callees first". You can print any crate's summaries:

```text
$ cargo psync summaries app
crate app
  Session::get_token
    write file "token"                       via std::fs::write
  sync_dirs
    write file                               via helpers::stamp -> std::fs::write
  tour
    write file "walk.log"                    via visit -> walk -> std::fs::write
    read file "walk.log"                     via read_log -> std::fs::read
  ...
```

Crates from crates.io are summarized too, with one difference. In a library, only *real* I/O
counts (files, processes, the environment). Inside an HTTP client, every request writes to a
socket, and counting that would make every call a write. At the boundary between your code and a
published API, the API's verb says what the call means. `kube::Api::list` reads, whatever bytes
it sends.

## Sound, precise, and what the driver gives up

An analysis that answers "what *may* this function do?" is a **may-analysis**. It is **sound**
if it never leaves out an effect that can happen, and **precise** if it never includes one that
cannot. No analysis of real programs is both, so the question is what to give up.

The driver gives up precision in these places:

- **Flow-insensitive.** It takes the union over every call in every basic block. A write behind
  `if false` still counts.
- **Unknown targets.** A path built at run time (`format!("{dir}/.synced")`) has no constant
  target, so it may be any file.
- **Whole processes.** Starting `git status` counts as touching everything.

It *cannot* see in these places, and marks the summary **opaque**:

- a call through a function pointer;
- a `dyn` call of a trait from another published crate (`dyn tower::Service`), which could be
  implemented by anything. The common I/O traits are partly an exception. A call of
  `AsyncWrite`, `io::Write`, `AsyncRead` or `io::Read` on any reader or writer that is not an
  in-memory buffer counts as a write or read *of that object* (named by its type). Through `dyn`,
  the summary is also marked opaque, because the implementation may do more;
- a trait method with no implementation in the build.

Opaque does not mean "assume the worst". It means "the compiler's effects are incomplete".
`cargo psync build` then keeps the macro's name-based guess as well as the compiler's effects. So
with facts the macro is at least as cautious as without them. The remaining assumptions are
listed in [What is checked, what is assumed](../internals/soundness.md).

Panics are a special case. Nearly every library function could panic somewhere, so the driver
counts only *explicit* panics in your own code (`unwrap`, `expect`, `panic!`, `assert!`,
`unreachable!`, `todo!`) as exits.

## Exercises

1. In the `summaries` example, add a function `sweep` whose own effect is `read net` (a read
   from the network) and which calls `visit`. Without running it, what is `S(sweep)`? Which SCC does `sweep` belong to?
2. Why can the cut-cycle method give the right answer for `walk` but the wrong one for `visit`?
   State in one sentence which member of a cycle the method gets right.
3. `fs::write(&a, ..)` and `fs::read(&b)`, with `a` and `b` variables: does psync keep them in
   order? Why? What would it take to let them overlap safely? (See the
   [roadmap](../safety/roadmap.md), item 1.)
4. A function calls `Command::new("ls").output()`. Which domain, access and target does that
   effect get, and what does it conflict with?
5. Why would counting socket writes inside libraries make the driver useless for kube-rs?

<details>
<summary>Answers</summary>

1. `S(sweep) = {read net, write file walk.log}`, from its own read and from `visit`, which
   reaches `walk`. `sweep` is its own SCC. Nothing calls back to it, so it is not in
   `{walk, visit}`.
2. The function where the depth-first walk *enters* the cycle is finished last, after every other
   member has been absorbed into it, so it alone sees the whole cycle. The other members were
   memoized while it was still incomplete.
3. Yes, they stay in order. Neither has a constant target, so either may be any file, and one of
   them writes. Overlapping them safely needs a *runtime* check: compare the two paths, run the
   concurrent schedule if they differ and the sequential one if they are equal.
4. Domain `Process`, access `Write`, no target. It conflicts with every other effect except a read
   of the environment, because the child can do anything to files, the network or services.
   (Recognizing `ls` as read-only is item 7 of the roadmap.)
5. Every API call (`list`, `get`, `watch`) sends bytes over a socket, so each one would be a write
   to the network domain. Every pair of calls would conflict, and no two requests could ever
   overlap, which is the opposite of what the driver is for.

</details>
