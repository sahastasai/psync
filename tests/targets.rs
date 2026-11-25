//! A prefetched loop whose items name their own files checks at run time that they do: with a
//! duplicate item, the loop runs as written.
use psync::{Executor, graph};
use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    path::Path,
    pin::pin,
    rc::Rc,
    task::Poll,
};

/// An asynchronous file system: each call is pending for a few polls, then does the real thing.
mod fs {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Calls in flight now, and the most at once.
    pub static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK: AtomicUsize = AtomicUsize::new(0);

    async fn pending(mut left: u32) {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        PEAK.fetch_max(ACTIVE.load(Ordering::SeqCst), Ordering::SeqCst);
        poll_fn(|cx| {
            if left == 0 {
                return Poll::Ready(());
            }
            left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await;
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }

    pub async fn read_to_string(path: impl AsRef<Path>) -> std::io::Result<String> {
        pending(3).await;
        std::fs::read_to_string(path)
    }

    /// Faster than a read: run at once, a write to the file being read lands first.
    pub async fn write(path: impl AsRef<Path>, data: &str) -> std::io::Result<()> {
        pending(1).await;
        std::fs::write(path, data)
    }
}

/// The tests share the counters in `fs`.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run<T: 'static>(future: impl Future<Output = T> + 'static) -> T {
    let executor = Executor::default();
    let out = Rc::new(RefCell::new(None));
    let o = out.clone();
    executor.spawn(1, async move {
        let value = pin!(future).await;
        *o.borrow_mut() = Some(value);
    });
    executor.run_until_stalled();
    out.borrow_mut().take().expect("finished")
}

/// Add one to the counter in each named file.
#[graph]
async fn bump(dir: std::path::PathBuf, names: Vec<&'static str>) -> std::io::Result<()> {
    for name in names {
        let path = dir.join(name);
        let old: u32 = fs::read_to_string(&path).await?.trim().parse().unwrap();
        std::fs::write(&path, (old + 1).to_string())?;
    }
    Ok(())
}

/// The same through a tuple pattern and a `format!` path.
#[graph]
async fn bump_numbered(dir: String, names: Vec<String>) -> std::io::Result<usize> {
    let mut seen = 0;
    for (i, name) in names.iter().enumerate() {
        let old: u32 = fs::read_to_string(format!("{dir}/{name}"))
            .await?
            .trim()
            .parse()
            .unwrap();
        std::fs::write(format!("{dir}/{name}"), (old + 1).to_string())?;
        seen = i + 1;
    }
    Ok(seen)
}

/// Read one file and write another. The tokens cannot tell `src` from `dst`; the paths are
/// compared when the function runs.
#[graph]
async fn read_then_write(src: &Path, dst: &Path) -> std::io::Result<String> {
    let old = fs::read_to_string(src).await?;
    #[psync::speculate]
    fs::write(dst, "new").await?;
    Ok(old)
}

fn counters(tag: &str, names: &[&str]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("psync-targets-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for n in names {
        std::fs::write(dir.join(n), "0").unwrap();
    }
    dir
}

fn count(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

#[test]
fn distinct_items_run_ahead_and_duplicates_run_in_order() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = counters("distinct", &["a", "b", "c"]);
    run(bump(dir.clone(), vec!["a", "b", "c"])).unwrap();
    assert_eq!(["a", "b", "c"].map(|n| count(&dir, n)), ["1", "1", "1"]);
    // Run ahead, both reads of `a` would see 0 and the file would end at 1.
    let dir = counters("duplicate", &["a", "b"]);
    run(bump(dir.clone(), vec!["a", "b", "a", "./a"])).unwrap();
    assert_eq!(count(&dir, "a"), "3");
    assert_eq!(count(&dir, "b"), "1");
    std::fs::remove_dir_all(&dir).unwrap();
    let dir = counters("format", &["a", "b"]);
    let names = ["a", "b", "a"].map(String::from).to_vec();
    assert_eq!(
        run(bump_numbered(dir.to_str().unwrap().into(), names)).unwrap(),
        3
    );
    assert_eq!(count(&dir, "a"), "2");
    std::fs::remove_dir_all(&dir).unwrap();
    // `dir/./a` is `dir/a`: paths are compared after lexical normalization.
    let dir = counters("dot", &["a", "b"]);
    run(bump(dir.clone(), vec!["a", "b", "./a"])).unwrap();
    assert_eq!(count(&dir, "a"), "2");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn computed_paths_overlap_only_when_they_differ() {
    use std::sync::atomic::Ordering;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = counters("paths", &["a", "b"]);
    let (a, b) = (dir.join("a"), dir.join("b"));
    fs::PEAK.store(0, Ordering::SeqCst);
    let old = run({
        let (a, b) = (a.clone(), b.clone());
        async move { read_then_write(&a, &b).await }
    });
    assert_eq!(old.unwrap(), "0");
    assert_eq!(
        fs::PEAK.load(Ordering::SeqCst),
        2,
        "different files overlap"
    );
    // One file: the write waits for the read, as written.
    fs::PEAK.store(0, Ordering::SeqCst);
    let same = dir.join(".").join("a");
    let old = run({
        let a = a.clone();
        async move { read_then_write(&a, &same).await }
    });
    assert_eq!(old.unwrap(), "0");
    assert_eq!(fs::PEAK.load(Ordering::SeqCst), 1);
    assert_eq!(count(&dir, "a"), "new");
    std::fs::remove_dir_all(&dir).unwrap();
}
