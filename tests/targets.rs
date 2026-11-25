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

    pub async fn read_to_string(path: impl AsRef<Path>) -> std::io::Result<String> {
        let mut left = 3;
        poll_fn(|cx| {
            if left == 0 {
                return Poll::Ready(());
            }
            left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await;
        std::fs::read_to_string(path)
    }
}

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
