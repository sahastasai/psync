# Classifying statements

`classify(stmt, scope, relaxed)` (`classify.rs`) decides what each statement is. The result is
either a `Node`, which carries everything the scheduler needs, or a `Barrier` with an optional
reason for the report.

## What a node records

```rust,ignore
pub(crate) struct Node {
    pub stmt: Stmt,               // the statement as written
    pub defs: Vec<(Ident, bool)>, // bindings it introduces, with their `mut` flag
    pub acc: Access,              // what it reads and writes (see the next chapter)
    pub awaits: bool,             // awaits outside nested closures and async blocks
    pub effect: bool,             // an ordinary statement: keeps its order with other effects
    pub exits: bool,              // contains ?, return, break or continue that leave it
    pub effects: Vec<Effect>,     // what it does to files, services, processes, the environment
    pub unclassified: Vec<String>,// awaited calls whose names say nothing (assumed to read)
    pub speculate: bool,          // #[psync::speculate]: its writes may run ahead of exits
    pub panics: bool,             // compiler facts: it may panic on purpose (an exit for speculation)
    pub independent: bool,        // #[psync::independent]: its effects commute with the region's
    pub label: String,            // for reports
    pub line: usize,
}
```

## Finding awaits and exits

The `Exits` visitor walks an expression. It does not enter closures, async blocks or nested items,
because their awaits and exits belong to them. It records:

- `awaits`, and the names of the awaited callees (`method` for `x.method()`, `Type::f` for a path
  call). A second list, `awaited_unscoped`, holds only the awaits outside any nested block. It is
  used to tell a guard that is kept from one that is dropped inside the statement.
- `exits`: any `?`, `return`, or `break`/`continue` that leaves the statement. Loops and labeled
  blocks inside the expression are tracked, so a `break` aimed at an inner loop does not count.
- a `blocker`: something psync cannot rewrite. That is an exit hidden in macro tokens, `bail!`-style
  macros, `yield`, or `break` with a value.

## The decision

In order, a statement is a **barrier** if:

1. it is a `let` without an initializer, or an item;
2. it carries `#[psync::seq]` or `#[psync::fresh]`;
3. `Exits` found a blocker;
4. it calls a clock function (`now`, `elapsed`, `duration_since`) outside closures and async blocks;
5. its pattern binds by `ref`;
6. it awaits a delay (`sleep`, `sleep_until`, `tick`, `yield_now`, `delay_for`), or awaits a guard
   (`lock`, `lock_owned`, `read`, `write`, `acquire`, `acquire_owned`, `acquire_many`, `begin`,
   `transaction`) outside a nested block.

Otherwise, it is a **node**. It is an **effect** unless it is *shaped* (a `let`, an `await` expression
statement, or a logging macro) and either awaits or binds a pure value. The block's last
expression, its value, is always made a barrier by the caller.

`relaxed` is used for a loop's prefix. It admits bindings of non-pure values as movable, because
the loop code has already confined the prefix to per-iteration data. An ordinary statement may
join a prefix only if it writes nothing, for example a guard like `if done { continue; }`.

## What is pure

`is_pure` accepts expressions with no side effects that are cheap to move:

- literals, paths, closures and ranges;
- field access, `&` (not `&mut`), unary and binary operators, casts, indexing, tuples, arrays and
  struct literals built from pure parts;
- method calls from a fixed list of read-only methods (`clone`, `to_string`, `len`, `iter`,
  `is_some`, ...);
- constructor calls: `T::new`, `T::from`, `T::default`, `with_capacity`, `Some`, `Ok`, `Err`, and
  tuple structs;
- `format!`, `vec!`, `concat!` and `stringify!` with pure arguments.

Everything else that does not await is an ordinary statement, an effect, and keeps its order.

## Labels

Report labels are made from the bindings and the first awaited callee (`user = user()?`). A
non-awaited binding shows the first 23 characters of its value. A trailing `?` means the node can
exit.
