//! Phase II: `#[psync::graph]` on synchronous functions. Statements marked `#[psync::par]` (or
//! recognized as blocking on the network or a process) run on threads beside independent ones,
//! with results, errors and panics as in sequential code.
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Blocks for `ms` milliseconds, then returns `value`, logging when it ran.
fn slow<T>(log: &Mutex<Vec<&'static str>>, name: &'static str, ms: u64, value: T) -> T {
    std::thread::sleep(Duration::from_millis(ms));
    log.lock().unwrap().push(name);
    value
}

#[psync::graph]
fn three_downloads(log: &Mutex<Vec<&'static str>>) -> (u32, u32, u32) {
    #[psync::par]
    let a = slow(log, "a", 100, 1);
    #[psync::par]
    let b = slow(log, "b", 100, 2);
    #[psync::par]
    let c = slow(log, "c", 100, a + 10);
    (a, b, c)
}

#[test]
fn independent_blocking_statements_overlap() {
    let log = Mutex::new(Vec::new());
    let start = Instant::now();
    assert_eq!(three_downloads(&log), (1, 2, 11));
    let elapsed = start.elapsed();
    // `a` and `b` overlap, `c` waits for `a`: two steps, not three.
    assert!(elapsed < Duration::from_millis(280), "{elapsed:?}");
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    let log = log.into_inner().unwrap();
    let pos = |x| log.iter().position(|n| *n == x).unwrap();
    assert!(pos("a") < pos("c"), "{log:?}");
}

#[psync::graph]
fn first_error_wins(log: &Mutex<Vec<&'static str>>) -> Result<u32, &'static str> {
    #[psync::par]
    let a: u32 = slow(log, "a", 100, Err("first"))?;
    #[psync::par]
    let b: u32 = slow(log, "b", 10, Err("second"))?;
    Ok(a + b)
}

#[test]
fn errors_are_replayed_in_program_order() {
    let log = Mutex::new(Vec::new());
    // `b` fails first in time, but sequential code would have stopped at `a`.
    assert_eq!(first_error_wins(&log), Err("first"));
}

#[psync::graph]
fn panic_after_error(log: &Mutex<Vec<&'static str>>, fail: bool) -> Result<u32, &'static str> {
    #[psync::par]
    let a: u32 = slow(log, "a", 50, if fail { Err("early") } else { Ok(1) })?;
    #[psync::par]
    let b: u32 = slow(log, "b", 10, None::<u32>).expect("b is missing");
    Ok(a + b)
}

#[test]
fn an_earlier_error_wins_over_a_later_panic() {
    let log = Mutex::new(Vec::new());
    assert_eq!(panic_after_error(&log, true), Err("early"));
}

#[test]
fn a_panic_keeps_its_payload() {
    let log = Mutex::new(Vec::new());
    let r = std::panic::catch_unwind(|| panic_after_error(&log, false));
    let payload = r.unwrap_err();
    // `expect` panics with a `String`; the payload arrives as raised, not re-wrapped.
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert_eq!(msg, "b is missing");
}

#[psync::graph]
fn guard_keeps_its_place(m: &Mutex<u32>, log: &Mutex<Vec<&'static str>>) -> u32 {
    #[psync::par]
    let a = slow(log, "a", 30, 1);
    let mut g = m.lock().unwrap();
    *g += 1;
    #[psync::par]
    let b = slow(log, "b", 30, 2);
    drop(g);
    a + b
}

#[test]
fn a_held_lock_is_a_barrier() {
    let (m, log) = (Mutex::new(0), Mutex::new(Vec::new()));
    assert_eq!(guard_keeps_its_place(&m, &log), 3);
    assert_eq!(*m.lock().unwrap(), 1);
}

/// A synchronous function returning a future: the async block inside is Phase I code.
#[psync::graph]
#[allow(clippy::manual_async_fn)]
fn returns_future(x: u32) -> impl std::future::Future<Output = u32> {
    async move {
        let a = async { x }.await;
        let b = async { x + 1 }.await;
        a + b
    }
}

#[test]
fn async_blocks_in_sync_functions_still_use_awaits() {
    let mut ex = psync::MultiExecutor::new(1);
    let _ = &mut ex;
    let v = std::thread::scope(|_| {
        let mut fut = std::pin::pin!(returns_future(2));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        loop {
            if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                break v;
            }
        }
    });
    assert_eq!(v, 5);
}

#[psync::graph]
fn guarded_writes(
    dir: &str,
    log: &Mutex<Vec<&'static str>>,
    fail: bool,
) -> Result<(), &'static str> {
    #[psync::par]
    if !std::path::Path::new(&format!("{dir}/one")).exists() {
        let data: u32 = slow(log, "one", 100, if fail { Err("download") } else { Ok(1) })?;
        std::fs::write(format!("{dir}/one"), data.to_string()).map_err(|_| "write")?;
    }
    #[psync::par]
    if !std::path::Path::new(&format!("{dir}/two")).exists() {
        let data: u32 = slow(log, "two", 100, Ok(2))?;
        std::fs::write(format!("{dir}/two"), data.to_string()).map_err(|_| "write")?;
    }
    Ok(())
}

#[psync::graph]
fn speculative_writes(
    dir: &str,
    log: &Mutex<Vec<&'static str>>,
    fail: bool,
) -> Result<(), &'static str> {
    #[psync::par]
    if !std::path::Path::new(&format!("{dir}/one")).exists() {
        let data: u32 = slow(log, "one", 100, if fail { Err("download") } else { Ok(1) })?;
        std::fs::write(format!("{dir}/one"), data.to_string()).map_err(|_| "write")?;
    }
    #[psync::par]
    #[psync::speculate]
    if !std::path::Path::new(&format!("{dir}/two")).exists() {
        let data: u32 = slow(log, "two", 100, Ok(2))?;
        std::fs::write(format!("{dir}/two"), data.to_string()).map_err(|_| "write")?;
    }
    Ok(())
}

#[test]
fn a_failed_download_stops_later_writes() {
    let dir = std::env::temp_dir().join(format!("psync-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();
    let log = Mutex::new(Vec::new());
    assert_eq!(guarded_writes(d, &log, true), Err("download"));
    // Sequential code never reached the second `if`, so its file is not written.
    assert!(!dir.join("two").exists());
    assert_eq!(guarded_writes(d, &log, false), Ok(()));
    assert!(dir.join("one").exists() && dir.join("two").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn speculative_writes_overlap_and_keep_the_error() {
    let dir = std::env::temp_dir().join(format!("psync-spec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();
    let log = Mutex::new(Vec::new());
    // The second write may happen (that is what `speculate` allows), but the error is the first.
    assert_eq!(speculative_writes(d, &log, true), Err("download"));
    let _ = std::fs::remove_file(dir.join("two"));
    let start = Instant::now();
    assert_eq!(speculative_writes(d, &log, false), Ok(()));
    assert!(
        start.elapsed() < Duration::from_millis(170),
        "{:?}",
        start.elapsed()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
