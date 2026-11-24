//! `MultiExecutor`: priorities across worker threads, live priority changes, bounded starvation
//! with fairness, and parallel speedup.
use psync::{MultiExecutor, yield_now};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

/// Occupy the executor's only worker until `gate` is released, so tasks spawned meanwhile queue
/// up and their order is decided by the scheduler alone.
fn gated(executor: &MultiExecutor) -> Arc<Barrier> {
    let gate = Arc::new(Barrier::new(2));
    let g = gate.clone();
    executor.spawn(u16::MAX, async move {
        g.wait();
    });
    gate
}

#[test]
fn highest_priority_first_fifo_among_equals() {
    let executor = MultiExecutor::new(1);
    let gate = gated(&executor);
    let log = Arc::new(Mutex::new(Vec::new()));
    for (priority, id) in [(1, 0), (8, 1), (8, 2), (3, 3)] {
        let log = log.clone();
        executor.spawn(priority, async move {
            log.lock().unwrap().push(id);
        });
    }
    gate.wait();
    executor.wait_idle();
    assert_eq!(*log.lock().unwrap(), [1, 2, 3, 0]);
}

#[test]
fn priority_changes_move_queued_tasks() {
    let executor = MultiExecutor::new(1);
    let gate = gated(&executor);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (a, b) = (log.clone(), log.clone());
    let low = executor.spawn(1, async move { a.lock().unwrap().push("low") });
    executor.spawn(2, async move { b.lock().unwrap().push("mid") });
    low.set_priority(5);
    gate.wait();
    executor.wait_idle();
    assert_eq!(*log.lock().unwrap(), ["low", "mid"]);
    assert!(low.is_finished());
}

/// A busy high-priority task that yields 200 times, and one low-priority task.
fn starve(executor: &MultiExecutor) -> usize {
    let gate = gated(executor);
    let log = Arc::new(Mutex::new(Vec::new()));
    let busy = log.clone();
    executor.spawn(10, async move {
        for i in 0..200 {
            busy.lock().unwrap().push(i);
            yield_now().await;
        }
    });
    let low = log.clone();
    executor.spawn(1, async move { low.lock().unwrap().push(usize::MAX) });
    gate.wait();
    executor.wait_idle();
    let log = log.lock().unwrap();
    log.iter().position(|v| *v == usize::MAX).expect("low ran")
}

#[test]
fn fairness_bounds_starvation() {
    // Strict priorities: the low task waits for all 200 steps.
    assert_eq!(starve(&MultiExecutor::new(1)), 200);
    // Every 4th pick takes the oldest ready task: the low task runs within a few steps.
    assert!(starve(&MultiExecutor::with_fairness(1, 4)) <= 4);
}

#[test]
fn workers_run_in_parallel() {
    fn spin(ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            std::hint::spin_loop();
        }
    }
    let executor = MultiExecutor::new(4);
    let start = Instant::now();
    for _ in 0..4 {
        executor.spawn(1, async { spin(100) });
    }
    executor.wait_idle();
    // Four 100 ms tasks on four workers: well under the 400 ms they take one after another.
    assert!(
        start.elapsed() < Duration::from_millis(300),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn block_on_returns_the_output() {
    let executor = MultiExecutor::new(2);
    let v = executor.block_on(3, async {
        yield_now().await;
        6 * 7
    });
    assert_eq!(v, 42);
}
