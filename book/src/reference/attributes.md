# Attributes and hints

## `#[psync::graph(...)]`

Goes on an `async fn`, including methods and `async fn main` (put it above `#[tokio::main]`). It
also rewrites async blocks and async closures inside the function it is on. A bare `#[graph]`
works when `psync::graph` is imported.

| Option | Default | Meaning |
|---|---|---|
| `bound = N` | `16` | at most `N` loop iterations' prefixes in flight, per loop; must be at least 1 |
| `explain` | off | print the dependency graph, loops and barriers to stderr at compile time |
| `off` | off | compile the function as written; hint attributes are removed |
| `crate = path` | `::psync` | path to the psync crate in generated code, for re-exports |
| `strict` | off | unclassified awaited calls are writes to an unknown resource: they conflict with every effect and never run speculatively |
| `speculate` | off | writes may run before earlier exits everywhere in the function |
| `yield_every = N` | off | insert `psync::yield_now().await` at the start of every loop body in async code, every `N` iterations ([Yield injection](../guide/executor.md#yield-injection)) |
| `effects(read(a, ..), write(b, ..), ordered(c, ..))` | none | extra verbs (whole names or first `_`-separated words) for the [effect analysis](../safety/effects.md) |

Options are comma-separated: `#[psync::graph(bound = 4, explain)]`. Anything else is a compile
error: ``expected `bound = N`, `explain`, `off`, `strict`, `speculate`, `effects(..)`, `yield_every = N` or `crate = path` ``.

## Hints

| Hint | On | Effect inside `#[psync::graph]` |
|---|---|---|
| `#[psync::seq]` | any statement | the statement is a barrier: it and everything around it keep their order; on a `for` loop, the loop is not prefetched |
| `#[psync::fresh]` | `let x = init;` with one binding | the `let` stays in place, and each concurrent chain that uses `x` begins with its own `let x = init;`; uses of `x` are not conflicts |
| `#[psync::independent]` | `for` or `while` loop | skip loop-carried-state and cross-iteration effect checks for prefetch and fission; required for `while` loops |
| `#[psync::independent]` | any other statement | its effects commute with those of the statements around it; data dependencies still apply |
| `#[psync::speculate]` | statement or `for` loop | its writes may run before earlier exits, or for later items before an earlier item ends the loop |

Outside a `#[psync::graph]` function, the hints expand to their input unchanged.

## Names psync treats specially

These lists are in `classify.rs` and `access.rs`.

| List | Names | Effect |
|---|---|---|
| guard awaits | `lock` `lock_owned` `read` `write` `acquire` `acquire_owned` `acquire_many` `begin` `transaction` | barrier, unless the guard is dropped inside the statement's own block |
| delay awaits | `sleep` `sleep_until` `tick` `yield_now` `delay_for` | barrier |
| clock calls | `now` `elapsed` `duration_since` | barrier (outside closures and async blocks) |
| ordered methods | `send` `send_all` `try_send` `recv` `try_recv` `next` `try_next` `feed` `write` `write_all` `flush` `read_exact` `read_to_end` `read_to_string` `shutdown` `close` `execute` `commit` `rollback` | a write to the receiver, whatever its type |
| logging macros | `trace` `debug` `info` `warn` `error` `log` | movable, and their `?`/`%` sigils are not exits |
| early-exit macros | `bail` `ensure` `try` `return_err` `fail` | barrier |
| pure macros | `format` `vec` `concat` `stringify` | pure if their arguments are |

The effect analysis described in [Side effects](../safety/effects.md) adds its own lists.
