# Side effects: files, network and processes

The analysis in [Reads and writes from tokens](../internals/access.md) sees the function's own
variables and fields. It cannot see the world outside the program. Take this function:

```rust,ignore
#[psync::graph]
async fn recreate(api: &Api, name: &str, obj: &Obj) -> Result<(), Error> {
    api.delete(name).await?;
    api.create(obj).await?;
    Ok(())
}
```

Neither statement writes a variable the other reads, and `api` is a shared `&` handle. Without
more information, psync would start the `create` before the `delete` finished, even though both act
on the same object on the server. It would also start the `create` when the `delete` fails, which
sequential code never does. The same goes for `fs::write("cfg.toml", ..)` followed by
`fs::read_to_string("cfg.toml")`: the file connects the two statements, but no variable does.

The effect analysis closes this gap. It gives every statement a list of **effects**, each one a
read or a write of a *target* in a *domain*. Statements whose effects conflict keep their order,
and a write never runs before an earlier statement that might leave.

## In numbers

| | |
|---|---|
| **23 of 89** | rewrites of real functions from the audit in which psync had run a write before an earlier exit, or reordered two operations on the same resource |
| **5** | of them need a one-line hint to rewrite at all (now 79 of 90 automatic and 10 with hints; 1 is still impossible) |
| **11** | functions in the kube-rs and DataFusion benchmark patches; 9 generate identical code, and 2 now keep their writes in order |
| **3.79×** | `graph_suite` geometric mean (3.82× before); 4 examples gained a `speculate` hint, and one keeps two uploads of the same file in order |

## Domains and targets

psync recognizes effects from the call itself. It sees tokens, not types, so it goes by paths and
names:

| Domain | Recognized from | Reads | Writes | Target |
|---|---|---|---|---|
| file | a path through `fs` (`std::fs::write`, `tokio::fs::read_to_string`), `File::create`/`File::open`, `OpenOptions`, `DirBuilder` | `read`, `read_to_string`, `read_dir`, `metadata`, `canonicalize`, `open`, ... | everything else (`write`, `create_dir_all`, `remove_file`, `rename`, `copy`, ...) | the path argument; `rename`/`copy` also the second |
| network | `TcpStream`, `UdpSocket`, `TcpListener`, `UnixStream`, `net::` | `connect`, `bind`, ... | `send_to`, `send`, `write`, `write_all` | the address |
| environment | `env::` | `env::var`, ... | `set_var`, `remove_var`, `set_current_dir` | the variable name |
| process | `Command::new`, `process::` | | always a write | conflicts with *every* domain (`cargo psync check`, which knows more, exempts a read of the environment) |
| service | any other **awaited** call | its name starts with a read verb: `get`, `list`, `head`, `fetch`, `query`, `describe`, ... | its name starts with a write verb: `create`, `put`, `post`, `patch`, `delete`, `upload`, `insert`, `update`, `commit`, ... | the receiver path (`api`, `self.s3`) is the domain; the verb's first argument is the target |

Some details:

- **The first verb in a method chain decides.** `client.get(url).header(..).send().await` reads.
  `s3.put_object().bucket(b).key(k).send().await` writes. A verb is the whole name or its first
  `_`-separated word, so `delete_namespaced_pod`, `put_blob` and `list_objects_v2` classify
  without any configuration.
- **Ordered verbs** (`send`, `append`, `push`, `publish`, `enqueue`, `emit`, `notify`, `ack`, ...)
  are writes whose relative order is observable. Two of them on one domain never reorder, whatever
  their targets.
- **Different receivers are different services.** `self.cache` and `self.db` do not conflict. An
  awaited *free* function (`delete_user(id).await`) reaches an unknown service, which may be behind
  any receiver. A receiver bound inside one statement (its own socket or client) is a different
  receiver from another statement's, even with the same name.
- **Plain synchronous calls count only when they reach a known domain.** `std::fs::write(..)` is a
  write. `map.get(k)` is not a service call, because it is not awaited.

The lists live in `psync-graph/src/effects.rs`.

## When two effects conflict

Two effects conflict when at least one writes, their domains may be the same, and their targets
may be the same. Targets are compared only as far as tokens allow:

- Two string literals are either equal or not: `"a.toml"` and `"b.toml"` differ. For files, a
  directory and the paths inside it overlap: `remove_dir_all("out")` conflicts with
  `read("out/a.txt")`.
- `format!("prefix{x}")` keeps its literal prefix. `format!("m.secret_storage.key.{id}")` cannot
  equal `"m.megolm_backup.v1"`, but may equal `"m.secret_storage.key.k1"`.
- Two `format!` strings whose placeholders are plain names or fields are compared piece by piece.
  `format!("{dir}/AppRun")` and `format!("{dir}/linuxdeploy")` agree up to `{dir}/` and then
  differ in their text, so they name different files. Within one region a name has one value
  (a statement that rebinds or changes it is ordered with both), so this is exact. They still
  conflict if they first differ at a placeholder (`{dir}` against `{other}`, which may hold the
  same text), if one is a directory of the other (`{dir}/a` and `{dir}/a/b`), if the format specs
  differ (`{dir:?}`), or if a placeholder is computed (`{}` of `self.base()`, which may return
  something new each call).
- A target that is just a name, `fs::write(&a, ..)`, takes the value of the block's latest
  `let a = ..;` before it, when that binding is not `mut` and its value is a literal, a
  `format!` or a join. `let a = dir.join("a.json")` names the file `{dir}/a.json`, the same
  template as `format!("{dir}/a.json")`. A join counts only when the name cannot be absolute:
  its text must start with a character other than `/` or `\`, and not with a drive letter such
  as `C:`, because `Path::join` with an absolute path replaces the base. The binding's value must
  not depend on a `mut` binding, and a later `let` of a name the value depends on forgets it.
- Anything else computed may be anything. In asynchronous code, two such file paths can still run
  concurrently, and they are compared at run time
  ([Paths compared at run time](../internals/schedule.md#paths-compared-at-run-time)).

`Path::new(x)`, `PathBuf::from(x)` and the like name what `x` names, so
`Path::new(&format!("{dir}/one")).exists()` reads the file `{dir}/one`.

Like literals, templates compare paths as text. Two different names in one directory are taken to
be different files, even though a symbolic link could make them one.

`path.exists()`, `is_file()`, `metadata()` and the other methods of `std::path::Path` that read
the file system are file reads of the receiver. They are also kept as reads of a service named by
the receiver, in case the receiver is some other type with a method of that name. Until this
change they were only the service read, so in synchronous code `if !p.exists() { .. }` and a
later `fs::write(&p, ..)` could run at once (test `path_exists_reads_the_file_system`).

Reads never conflict with reads. Here is a read-check-recreate function:

```rust,ignore
#[psync::graph]
async fn replace(api: &Api, limits: &Limits, name: &str, obj: &Obj) -> Result<(), Error> {
    let old = api.get(name).await?;
    let quota = limits.get(obj.ns).await?;
    api.delete(name).await?;
    api.create(obj).await?;
    Ok(())
}
```

and what psync builds for it:

```text
  region 0:
    n0: old = get()? [await] {read `api` `name` (get)}
    n1: quota = get()? [await] {read `limits` `obj . ns` (get)}
    n2: delete()? [await] {write `api` `name` (delete)}
    n3: create()? [await] {write `api` `obj` (create)}
    n0 -> n2 (write `api` `name` (delete) vs read `api` `name` (get))
    n1 -> n2 (may exit before `delete` runs)
    n0 -> n3 (write `api` `obj` (create) vs read `api` `name` (get))
    n1 -> n3 (may exit before `create` runs)
    n2 -> n3 (write `api` `name` (delete) vs write `api` `obj` (create))
    level 0: n0 || n1
    level 1: n2
    level 2: n3
```

The two reads still overlap. The `delete` waits for both, and the `create` waits for the `delete`.

Files work the same way:

```text
  region 0:
    n0: fs::write()? [await] {write file "app/cfg.toml" (fs::write)}
    n1: cfg = fs::read_to_string()? [await] {read file "app/cfg.toml" (fs::read_to_string)}
    n2: readme = fs::read_to_string()? [await] {read file "README.md" (fs::read_to_string)}
    n0 -> n1 (write file "app/cfg.toml" (fs::write) vs read file "app/cfg.toml" (fs::read_to_string))
    level 0: n0->n1 || n2
```

## Writes are not speculated

[Early exits](../guide/exits.md) explained that psync may start an await before it knows whether an
earlier `?` leaves the function. For a read that is harmless. For a write it is not, because the
sequential program would not have written at all. So:

- **In a region**, a statement with a write effect waits for every earlier statement that may
  exit (``may exit before `delete` runs``).
- **In a loop**, prefetching starts the prefixes of later items before earlier items finish. In
  fact it runs *every* item's prefix, as [Runtime support](../internals/runtime.md) explains. If
  the body can end the loop (`?`, `return`, `break`, or a labeled `continue` to an outer loop),
  a prefix with a write is not prefetched:

  ```text
  loop `for f in files`: would write `store` `f` (upload) for later items before knowing whether an
  earlier item ends the loop (`#[psync::speculate]` allows it)
  ```

- **In fission**, the first part runs for every item before the rest runs for any item. So it may
  not contain writes when the body can end the loop.

## Writes across iterations

Prefetching also reorders iterations against each other: item 3's prefix may run before item 2's
rest. psync assumes that **targets computed from the loop item name different objects in
different iterations**. `store.upload(f)` for `f in files` touches a different object per item. A
target that does not mention the item, such as a literal, a field or an unknown free function, is
the same in every iteration:

- a prefetched write to such a target is refused (`would write ... in every iteration at once`);
- a prefetched read that conflicts with such a write elsewhere in the body is refused too.

If two items can name the same object, for example a list with duplicates, the assumption is
false. For files, psync checks it at run time. When every per-item path can be evaluated ahead
(`dir.join(name)`, `format!("{name}.json")`), the generated code compares the paths of all items
first and runs the loop as written if two items share a file that one of them writes
([Loop rewriting](../internals/loops.md#checking-per-item-files)). For other targets, or a path that
comes from awaited work, deduplicate first (the fission pattern from `jdx/mise` does exactly
this), or mark the loop `#[psync::seq]`.

## Hints and options

| Hint or option | Says | Effect |
|---|---|---|
| `#[psync::speculate]` on a statement or `for` loop | running these writes when sequential code would not have is harmless (idempotent uploads, cache fills, PUTs of the same value) | lifts the speculation rule only |
| `#[psync::graph(speculate)]` | the same, for the whole function | |
| `#[psync::independent]` on a statement | its effects commute with the statements around it (two writes to different objects through one client) | lifts effect conflicts for that statement; data dependencies stay |
| `#[psync::independent]` on a loop | iterations do not depend on each other | lifts the cross-iteration rules; the speculation rule stays |
| `#[psync::graph(effects(read(..), write(..), ordered(..)))]` | the listed names (whole names or first words) read, write, or write in order | extends the verb lists, for your own APIs |
| `#[psync::graph(strict)]` | unclassified awaited calls may do anything | see below |

Hints are assertions. psync does not check them.

## Unclassified calls and `strict`

An awaited call whose name has no known verb, such as `client.request(req)` or `do_work(x)`, is
*unclassified*. By default it is assumed to read, which is how psync behaved before effects
existed. `explain` lists every such call, so you can see what you are trusting:

```text
  unclassified (assumed to read; `strict` treats them as writes): request
```

Under `#[psync::graph(strict)]`, an unclassified call is a write to an unknown resource. It
conflicts with every effect and never runs speculatively. Use `strict` where a wrong guess is
expensive, and teach psync your API's names with `effects(..)` to get the concurrency back.

## What the audit found

Re-running psync over the 90 confirmed real-world findings showed the analysis at work.

**Five rewrites now need a hint:**
- `matrix-rust-sdk` `delete_all_known_secrets` and `mark_backup_as_enabled` write account data in
  a loop or one after another, behind `?`.
- `apache/iggy` `route_data` writes Iceberg tables in a loop behind `?`.
- `zed` `open` (file diff view) performs two `project.update(..)` calls.
- `zed` `get_git_committer` runs `git.run(..)` twice, and `run` counts as a write verb.

**Eighteen more** keep their concurrency but now order a write. Among them are deletes in
`neon`'s physical GC, `put`s in `delta-rs`, uploads in `mise` and GreptimeDB, `send_to_device` in
`matrix-rust-sdk`, `post_validator_voluntary_exit` in Lighthouse, `create_dir_all` in Quickwit, and
`shutdown` in DataFusion's writer orchestration.

Each of these 23 was a place where the earlier psync could perform a write the sequential program
would not have, or reorder two operations on one resource.

The analysis is also conservative where names mislead. DataFusion's
`ListingSchemaProvider::refresh` calls `self.factory.create(..)`, which builds a table provider in
memory. psync reads `create` as a write and no longer prefetches that loop. One line restores it:
`#[psync::speculate]` on the loop, or `effects(read(create))` on the function.

## Limits

This is still a token-level analysis:

- **Names can lie.** A `get` that writes, or an unclassified call that deletes, is trusted as named.
  `strict` removes the second risk. Only types can remove the first (see the
  [roadmap](./roadmap.md)).
- **Aliasing outside the program is invisible.** Two receivers that reach the same server, a
  symlink, or two paths to one file are treated as different.
- **Callees are opaque to the macro.** An awaited call to your own helper is classified by the
  helper's name, not by what its body does. [`cargo psync check`](./check.md) reads the bodies and
  reports where that guess was wrong.
- **Panics are not exits, from tokens alone.** The macro cannot tell which calls may panic, so
  a write may still run ahead of an earlier call that panics. The compiler can:
  [`cargo psync check`](./check.md) reports such a write, and with
  [compiler facts](./facts.md) the macro keeps it in order.

## Exercises

Predict the edges, then check with `psync-expand FILE LINE`.

1. ```rust,ignore
   async fn f1() -> std::io::Result<()> {
       tokio::fs::remove_dir_all("out").await?;
       let a = tokio::fs::read("out/a.txt").await?;
       let b = tokio::fs::read("cfg.toml").await?;
       Ok(())
   }
   ```
2. ```rust,ignore
   async fn f2(client: &Client, id: &str) -> Result<(), Error> {
       let backup = client.get_account_data("m.megolm_backup.v1").await;
       client.put_account_data(format!("m.secret_storage.key.{id}"), "{}").await;
       let key = client.get_account_data("m.secret_storage.key.k1").await;
       Ok(())
   }
   ```
3. ```rust,ignore
   async fn f3(&self, id: u64) -> Result<(), Error> {
       let user = self.db.get_user(id).await?;
       self.cache.put(id, &user).await;
       let other = self.db.get_user(id + 1).await?;
       delete_user(id).await;
       let again = self.db.get_user(id).await?;
       Ok(())
   }
   ```
   Why does `delete_user` conflict with both `get_user(id)` and `get_user(id + 1)`?
4. ```rust,ignore
   async fn f4(api: &Api) -> Result<(), Error> {
       let cfg = api.get_config("main").await;
       let out = tokio::process::Command::new("ls").output().await;
       let home = std::env::var("HOME");
       let more = api.get_config("other").await;
       Ok(())
   }
   ```

<details>
<summary>Answers</summary>

1. One edge, ``n0 -> n1 (write file "out" (fs::remove_dir_all) vs read file "out/a.txt" (fs::read))``:
   a directory contains the paths inside it. `cfg.toml` is neither, so the level is
   `n0->n1 || n2`. The reads may run before `remove_dir_all` returns, because reads are not
   held back by an earlier `?`.
2. Only `n1 -> n2`. The template `m.secret_storage.key.{id}` keeps its literal prefix, so it cannot
   be `m.megolm_backup.v1`, but it may be `m.secret_storage.key.k1`. The level is `n0 || n1->n2`.
3. Three edges into `delete_user` (`n3`), from `n0`, `n1` and `n2`, and one out of it, to
   `again`. An awaited free function
   reaches an unknown service (``write service `id` (delete_user)``), which may be behind any
   receiver. Its target `id` is computed, and so are `id` and `id + 1`, which tokens cannot tell
   apart, so it conflicts with every read and write around it. The
   `cache.put` waits for `user` (data), and `get_user(id)` and `get_user(id + 1)` run together.
   The levels are `n0 || n2`, then `n1`, then `n3->n4`.
4. A process conflicts with every domain: it could read or write anything. So
   `n0 -> n1`, `n1 -> n2` and `n1 -> n3`. The levels are `n0->n1`, then `n2 || n3`. The two
   `get_config` reads would have run together without the command. (`cargo psync check` knows
   more, and lets the environment read through.)

</details>
