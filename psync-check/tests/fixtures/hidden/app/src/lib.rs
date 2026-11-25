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
