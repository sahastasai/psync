//! Rewrites that look safe from their tokens but are not, and one that is.
use psync::graph;
use std::cell::RefCell;

#[derive(Debug)]
pub struct Error;
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Error
    }
}

/// `warm` has no verb, so the macro assumes it reads; its body writes the file `load` reads.
async fn warm() -> std::io::Result<()> {
    std::fs::write("cache.json", b"{}")
}
async fn load() -> std::io::Result<String> {
    std::fs::read_to_string("cache.json")
}

#[graph]
pub async fn start() -> Result<String, Error> {
    warm().await?;
    let s = load().await?;
    Ok(s)
}

/// `get_token` says it reads, but it writes.
pub struct Session;
impl Session {
    async fn check(&self) -> Result<(), Error> {
        Ok(())
    }
    async fn get_token(&self) -> String {
        let _ = std::fs::write("token", "t");
        "t".into()
    }
}

#[graph]
pub async fn login(s: &Session) -> Result<String, Error> {
    s.check().await?;
    let t = s.get_token().await;
    Ok(t)
}

/// A loop whose prefix writes through a dependency, in a body that can end the loop.
#[graph]
pub async fn sync_dirs(dirs: Vec<String>) -> Result<(), Error> {
    for d in dirs {
        helpers::stamp(&d).await?;
    }
    Ok(())
}

/// Safe: two reads of different files.
#[graph]
pub async fn sizes() -> Result<usize, Error> {
    let a = helpers::inspect("a.txt").await?;
    let b = helpers::inspect("b.txt").await?;
    Ok(a + b)
}

/// Two chains share a `RefCell`: their updates interleave.
async fn bump(log: &RefCell<Vec<u32>>, v: u32) -> u32 {
    log.borrow_mut().push(v);
    v
}
#[graph]
pub async fn shared(log: &RefCell<Vec<u32>>) -> u32 {
    let a = bump(log, 1).await;
    let b = bump(log, 2).await;
    a + b
}

/// `create_view` sounds like a write but only reads: by name the two calls stay in order, with the
/// compiler's facts (`cargo psync build`) they overlap.
pub struct Catalog;
impl Catalog {
    async fn create_view(&self, name: &str) -> Result<usize, Error> {
        Ok(std::fs::read(name)?.len())
    }
}

#[graph]
pub async fn views(c: &Catalog) -> Result<usize, Error> {
    let a = c.create_view("a.sql").await?;
    let b = c.create_view("b.sql").await?;
    Ok(a + b)
}

/// A trait object whose implementation writes the file the next call reads. Nothing in the
/// function's tokens says so, and the compiler cannot resolve `dyn` at the call: the link step
/// resolves it over every implementation in the build.
pub trait Store {
    fn checkpoint(&self) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<()>> + '_>>;
}
pub struct FileStore;
impl Store for FileStore {
    fn checkpoint(&self) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<()>> + '_>> {
        Box::pin(async { std::fs::write("store.db", b"x") })
    }
}
async fn inspect_db() -> std::io::Result<usize> {
    Ok(std::fs::read("store.db")?.len())
}

#[graph]
pub async fn snapshot(store: &dyn Store) -> Result<usize, Error> {
    store.checkpoint().await?;
    let n = inspect_db().await?;
    Ok(n)
}

/// Mutual recursion: `walk` writes `walk.log` and calls `visit`, which calls `walk` back. The
/// summary of `visit` must include `walk`'s write, whichever of the two is summarized first.
async fn walk(n: u32) -> std::io::Result<()> {
    std::fs::write("walk.log", b"x")?;
    if n > 0 {
        Box::pin(visit(n - 1)).await?;
    }
    Ok(())
}
async fn visit(n: u32) -> std::io::Result<()> {
    if n > 0 {
        Box::pin(walk(n - 1)).await?;
    }
    Ok(())
}
async fn read_log() -> std::io::Result<usize> {
    Ok(std::fs::read("walk.log")?.len())
}
async fn read_notes() -> std::io::Result<usize> {
    Ok(std::fs::read("notes.txt")?.len())
}

/// Checked first, without findings (`read_notes` reads another file, after `walk`'s exit):
/// summarizes `walk`, and inside the cycle `visit`, before `tour` asks for `visit`.
#[graph]
pub async fn walk_then_read() -> Result<usize, Error> {
    walk(1).await?;
    let a = read_notes().await?;
    Ok(a)
}

/// `visit` writes `walk.log` through `walk`: `read_log` conflicts with it.
#[graph]
pub async fn tour() -> Result<usize, Error> {
    visit(2).await?;
    let n = read_log().await?;
    Ok(n)
}

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

/// `parse_config` panics if `app.toml` is missing (`unwrap`): sequential code never writes the
/// stamp then, but psync starts `stamp_run` alongside it. Nothing in `boot`'s tokens shows an exit.
async fn parse_config() -> usize {
    std::fs::read_to_string("app.toml").unwrap().len()
}
async fn stamp_run() -> std::io::Result<()> {
    std::fs::write("run.stamp", b"1")
}

#[graph]
pub async fn boot() -> Result<usize, Error> {
    let n = parse_config().await;
    stamp_run().await?;
    Ok(n)
}

/// `stash` has no verb; it writes the file it is given, through another function. The compiler
/// says which parameter it writes (per-argument effects), so each call writes its own file.
fn stash(path: impl AsRef<std::path::Path>, data: &str) -> std::io::Result<()> {
    write_it(path.as_ref(), data)
}
fn write_it(p: &std::path::Path, d: &str) -> std::io::Result<()> {
    std::fs::write(p, d)
}

/// Safe: two different files.
#[graph]
pub fn stash_both() -> std::io::Result<()> {
    #[psync::par]
    let r1 = stash("a.json", "a");
    #[psync::par]
    let r2 = stash("b.json", "b");
    r1.and(r2)
}

/// The same file twice: the second write must stay second.
#[graph]
pub fn stash_same() -> std::io::Result<()> {
    #[psync::par]
    let r1 = stash("a.json", "1");
    #[psync::par]
    let r2 = stash("a.json", "2");
    r1.and(r2)
}

/// The same through an `async fn`: its parameters live in its coroutine (captured, then saved
/// across the `await`), and the compiler still says which one it writes.
async fn ready() {}
async fn keep(path: &str, data: &str) -> std::io::Result<()> {
    ready().await;
    std::fs::write(path, data)
}

/// Safe: two different files.
#[graph]
pub async fn keep_both() -> std::io::Result<()> {
    let a = keep("x.json", "1").await;
    let b = keep("y.json", "2").await;
    a.and(b)
}

/// The same file twice.
#[graph]
pub async fn keep_same() -> std::io::Result<()> {
    let a = keep("x.json", "1").await;
    let b = keep("x.json", "2").await;
    a.and(b)
}

/// Safe: the exit `r1?` comes after the second write in program order, so running that write
/// early is what sequential code does too.
#[graph]
pub fn stash_tail() -> std::io::Result<()> {
    #[psync::par]
    let r1 = stash("c.json", "c");
    #[psync::par]
    let r2 = stash("d.json", "d");
    r1?;
    r2
}

/// Not rewritten: the dependence graph shows that the write runs only after the `?` and inside
/// the `if`.
pub async fn guarded(path: &str) -> std::io::Result<()> {
    let n = std::fs::read_to_string(path)?.len();
    ready().await;
    if n > 3 {
        std::fs::write(path, "long")?;
    }
    Ok(())
}

/// Not rewritten: the read's result flows, across an await, into the write; `ready` uses nothing.
pub async fn chained(path: &str, out: &str) -> std::io::Result<()> {
    let s = std::fs::read_to_string(path)?;
    ready().await;
    std::fs::write(out, s)?;
    Ok(())
}

// Hidden state: a static atomic, a static `Mutex`, a `thread_local!` and a `Mutex` behind a
// reference, each touched by two statements that psync runs on two threads. Nothing in the
// tokens says so; the compiler's facts do, `cargo psync check` reports each pair
// (`shared-state`), and the certificates name the state (`h/app/HITS`, `n/log/$state`).

static HITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static MISSES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static JOURNAL: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
thread_local! {
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
/// Counted on purpose: certificates list it and hide it, the check does not report it.
#[psync::observed]
static PROBES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn hit() -> u32 {
    HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}
fn miss() -> u32 {
    MISSES.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}
fn note(v: u32) -> usize {
    match JOURNAL.lock() {
        Ok(mut j) => {
            j.push(v);
            j.len()
        }
        Err(_) => 0,
    }
}
fn enter() -> u32 {
    DEPTH.with(|d| {
        d.set(d.get() + 1);
        d.get()
    })
}
fn tally(log: &std::sync::Mutex<Vec<u32>>, v: u32) -> usize {
    match log.lock() {
        Ok(mut l) => {
            l.push(v);
            l.len()
        }
        Err(_) => 0,
    }
}
fn probe() -> u32 {
    PROBES.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// A static atomic, twice: the second count must see the first.
#[graph]
pub fn counted() -> u32 {
    #[psync::par]
    let a = hit();
    #[psync::par]
    let b = hit();
    a + b
}

/// A static `Mutex`, twice: the journal's order is the program's.
#[graph]
pub fn journaled() -> usize {
    #[psync::par]
    let a = note(1);
    #[psync::par]
    let b = note(2);
    a + b
}

/// A thread-local, twice: on two threads, each call would see its own counter.
#[graph]
pub fn nested() -> u32 {
    #[psync::par]
    let a = enter();
    #[psync::par]
    let b = enter();
    a + b
}

/// A `Mutex` behind a reference, twice.
#[graph]
pub fn appended(log: &std::sync::Mutex<Vec<u32>>) -> usize {
    #[psync::par]
    let a = tally(log, 1);
    #[psync::par]
    let b = tally(log, 2);
    a + b
}

/// Safe: two different statics.
#[graph]
pub fn hit_and_miss() -> u32 {
    #[psync::par]
    let a = hit();
    #[psync::par]
    let b = miss();
    a + b
}

/// Safe but for observed state: two probes of a counter marked `#[psync::observed]`.
#[graph]
pub fn probed() -> u32 {
    #[psync::par]
    let a = probe();
    #[psync::par]
    let b = probe();
    a.max(b)
}
