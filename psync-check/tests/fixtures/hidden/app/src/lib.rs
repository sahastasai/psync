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
