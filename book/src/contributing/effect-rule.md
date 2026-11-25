# Adding an effect rule

An *effect rule* teaches psync what a call does to the outside world. There are two places for
rules, one for each half of psync, and a complete change usually touches both:

| Where | Decides from | Used by |
|---|---|---|
| `psync-graph/src/effects.rs` | the call's **tokens**: path, method name, literal arguments | the macro, always |
| `psync-check/src/driver/analysis.rs`, `io_effect` and its neighbours | the **resolved callee path** in MIR, and constant arguments | `cargo psync check`, and the macro through facts |

If the two halves disagree, the danger is in the driver. Under `cargo psync build`, a compiler
fact *replaces* the macro's own guess for that call. So a driver rule that misses an effect the
macro would have seen makes psync less safe with facts than without them.

## The shape of a rule

Both halves describe an effect the same way: a **domain** (file system, network, environment,
process, output, or a named service), an **access** (read, write, or ordered write) and an
optional **target** (a path or key from a string literal). Two effects conflict when at least
one writes, the domains match, and the targets may be equal (`psync_check::conflict`; see
[Effects and summaries](../foundations/effects.md)).

## Worked example: file operations on two paths

This is a real change, made while this chapter was written.

**The bug.** The driver's `io_effect` recognizes `std::fs::copy` as a file write and takes the
target from the first argument:

```rust,ignore
std::fs::copy("draft.html", "site.html")    // driver said: write file "draft.html"
```

That is wrong twice over. `copy` *reads* its first path and *writes* its second. So a read of
`site.html` running at the same time was not reported as a conflict. With facts, the macro
would even schedule it concurrently. `rename`, `hard_link` and `symlink` had the same problem.
The macro's own rule in `effects.rs` already pushed a second write for these functions:

```rust,ignore
// `rename(from, to)`, `copy(from, to)`, `hard_link(src, dst)` also touch the second.
if kind == Kind::Write && c.args.len() >= 2
    && matches!(last.as_str(), "rename" | "copy" | "hard_link" | "symlink" | "soft_link")
{
    self.push(Domain::Fs, Kind::Write, target_of(c.args.iter().nth(1)), op, false);
}
```

**Step 1: write the failing test first.** Effects hidden in a callee are the driver's job, so
the test belongs in the driver's fixture, `psync-check/tests/fixtures/hidden/app/src/lib.rs`.
Choose names that are *not* verbs the macro knows (`grep` the verb lists in `effects.rs`).
Otherwise the macro orders the calls by itself, there is nothing concurrent left to check, and
the test proves nothing. `publish` is a known verb, so the fixture uses `promote`:

```rust,ignore
/// `fs::copy` writes its *second* path: `promote` writes `site.html`, which `read_site` reads.
async fn promote() -> std::io::Result<()> {
    std::fs::copy("draft.html", "site.html").map(|_| ())
}
async fn read_site() -> std::io::Result<usize> {
    Ok(std::fs::read("site.html")?.len())
}

#[graph]
pub async fn release() -> Result<usize, Error> {
    promote().await?;
    let n = read_site().await?;
    Ok(n)
}
```

Then, in `psync-check/tests/fixture.rs`, assert the new finding, raise the total, and add
`release` to the functions that facts must keep sequential:

```rust,ignore
assert!(
    app.findings.iter().any(|f| f.kind == "conflict"
        && f.function == "release"
        && f.message.contains("write file \"site.html\"")),
    "{:#?}", app.findings
);
```

Run `cargo test` in `psync-check/` and watch it fail.

**Step 2: the rule.** In `analysis.rs`, next to `io_effect`:

```rust,ignore
/// File operations on two paths, which write the second: the access to the first.
fn two_path_fs(name: &str) -> Option<Access> {
    match name {
        "copy" => Some(Access::Read),
        "rename" | "hard_link" | "symlink" | "soft_link" | "symlink_file" | "symlink_dir" => {
            Some(Access::Write)
        }
        _ => None,
    }
}
```

And where `call` applies `io_effect`, it records two effects for these functions, reading the
second constant argument with `string_constant`:

```rust,ignore
let second = (domain == Domain::Fs && keeps_target).then(|| two_path_fs(&name)).flatten();
match second {
    Some(first_access) => {
        let to = args.get(1).and_then(|a| self.string_constant(a));
        add(Domain::Fs, first_access, target);
        add(Domain::Fs, Access::Write, to);
    }
    None => add(domain, access, if keeps_target { target } else { None }),
}
```

Note two choices that keep the rule sound:

- **When unsure, write.** `hard_link` changes the original's link count and `symlink` may
  replace an existing link, so they count as writes on both paths. Only `copy`, which certainly
  does not change its source, gets a read.
- **A missing constant means "any file".** If the second argument is not a string literal, `to`
  is `None`, and the write conflicts with every file effect.

**Step 3: run everything.** `cargo test` in `psync-check/` now passes, and `cargo psync fix` on
the fixture inserts one more hint. Then run `cargo psync check` on a real project (the
benchmark branches of kube-rs and DataFusion are what the book's numbers come from), because a new rule can
create warnings in code that was clean.

**Step 4: the book.** Say what changed where users will look for it. Here that is the list of
recognized operations in [Side effects](../safety/effects.md) and the fixture counts in
[Scheduling with compiler facts](../safety/facts.md).

## Other kinds of rules

| You want to | Change | Test with |
|---|---|---|
| a new **verb** (`refresh` reads) | `READ_VERBS` / `WRITE_VERBS` / `ORDERED_VERBS` in `effects.rs`; the driver reuses them via `psync_graph::verb_of` for other crates' APIs | a unit test in `effects.rs` (`report(..)`, assert the edge) |
| a new **I/O function of std or a runtime** | `io_effect` in `analysis.rs`, and the matching token rule in `effects.rs` | the fixture |
| a new **crate whose I/O is known by path** (like tokio's) | `IO_CRATES` in `analysis.rs` | the fixture, with that crate as a dependency |
| a new **barrier** (an await whose position matters) | `DELAY_AWAITS` / `GUARD_AWAITS` in `classify.rs` | a `tests/graph.rs` test that checks timing and order |
| a new **pure method** (may move freely) | `PURE_METHODS` in `access.rs` | a unit test asserting no edge |

Users can also extend the verbs without changing psync, with
`#[psync::graph(effects(read(..), write(..), ordered(..)))]`. A rule belongs in psync itself when
it holds for everyone who uses that API.
