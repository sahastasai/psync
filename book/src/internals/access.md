# Reads and writes from tokens

The heart of the dependency analysis is `access::scan`. It approximates what a statement reads
and writes from its tokens alone. There are no types and no name resolution.

## Access paths

```rust,ignore
pub(crate) struct Access {
    pub uses: Set,    // every variable-like identifier mentioned (drives def-use edges)
    pub writes: Set,  // root identifiers of written paths
    pub rpaths: Set,  // access paths read, such as `self.client`
    pub wpaths: Set,  // access paths written, such as `self.groups`
}
```

For each identifier that could be a variable, `scan` follows `.field` and `[index]` to the place
being accessed. That gives `self.groups`, `conn.inner` or `buf`. Identifiers right after a `.`,
before `::`, lifetimes and keywords are skipped. Identifiers inside `format!`-style string literals
(`"{name}"`) count as reads.

Paths are compared by prefix. `self.groups` overlaps `self` and `self.groups.len`, but not
`self.group`. So a loop can write `self.groups` while its prefetched requests read `self.client`.

## What counts as a write

A path is written when:

| Form | Example |
|---|---|
| `&mut path` | `read_row(&mut conn, 1)` |
| assignment or compound assignment | `total += x`, `self.state = s` |
| a method call on a `mut` binding or `&mut` handle, unless the method looks read-only | `v.push(x)` on `let mut v` |
| an **ordered** method on any receiver | `tx.send(v)`, `stream.next()`, `file.write_all(b)` |
| a `&mut` handle passed along whole | `helper(conn)` where `conn: &mut Connection` |

The ordered methods keep their relative order on one receiver even through `&self`:

```text
send send_all try_send recv try_recv next try_next feed write write_all flush
read_exact read_to_end read_to_string shutdown close execute commit rollback
```

A method counts as read-only if it is in the pure list (`clone`, `len`, `iter`, `is_some`, ...) or
starts with one of these prefixes:

```text
is_ has_ get as_ to_ contains len iter keys values first last count unwrap expect
starts_with ends_with clone map ok err list fetch find load query lookup describe
stat head exists search resolve download retrieve
```

The prefix list only matters for receivers that psync already knows are mutable. A call like
`v.get(k)` on `let mut v` stays a read. `v.insert(k, x)` is a write.

## Which statement owns which names

Bindings made inside a statement, such as match arms or nested `let`s, belong to that statement.
They are removed from its writes, so a match arm binding `x` does not conflict with an outer `x`.
Uses of `#[psync::fresh]` bindings are removed entirely, because each branch makes its own copy.

## Edges

`schedule::depends(a, b)`, for an earlier `a` and a later `b`, adds an edge for the first of:

1. **data flow**: `b` uses a name that `a` defines;
2. **rebinding**: both define the same name;
3. **shadowing**: `b` defines a name that `a` uses;
4. **conflict**: `a` writes a path that overlaps something `b` reads or writes, or `b` writes a path
   that `a` reads;
5. **side-effect conflict**: their effects on files, services or other outside resources conflict
   (`effects::conflicts`, see [Side effects](../safety/effects.md)), unless either is marked
   `#[psync::independent]`. The reason names both effects;
6. **exit before a write**: `a` may exit or panic and `b` writes something outside the program,
   unless `b` is marked `#[psync::speculate]`. The reason is ``may exit before `op` runs``;
7. **effect order**: both are ordinary statements.

Two reads of the same path never conflict.

## What the token analysis cannot see

These limits come from working on tokens. Each one is either guarded by the compiler, or covered
by a rule above:

- **Aliasing through shared references.** `a.f()` and `b.g()` on two `&` handles to the same
  `RefCell` look independent. A `RefCell` borrowed mutably twice at once panics at run time. A
  `Mutex` serializes, and an async lock is a barrier anyway.
- **Hidden `&mut`.** A method taking `&mut self` on a binding not declared `mut` looks like a read.
  If two such calls overlap, the borrow checker rejects the generated code.
- **Effects behind calls.** `store.put(k, v).await` on a shared handle is neither ordered nor
  `&mut`. The [side-effects chapter](../safety/effects.md) covers how psync recognizes such calls.
- **Method names are conventions.** A method called `get_and_increment` starts with `get`.
  Read-only prefixes only apply to receivers already known to be mutable, so this mostly matters
  for `mut` bindings.
