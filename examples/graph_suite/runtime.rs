//! A host runtime for the examples: psync's executor with a condition-variable park and a timer
//! thread, so simulated I/O sleeps in real time and the executor idles between wakes.

use psync::{Executor, Park, Pool, Task};
use std::{
    cell::RefCell,
    collections::BinaryHeap,
    future::Future,
    pin::pin,
    rc::Rc,
    sync::{Condvar, LazyLock, Mutex},
    task::{Poll, Waker},
    time::{Duration, Instant},
};

/// Parks the executor's thread until a waker fires.
#[derive(Default)]
pub struct ThreadPark;

static SIGNAL: LazyLock<(Mutex<bool>, Condvar)> = LazyLock::new(Default::default);

impl Park for ThreadPark {
    fn park(&self) {
        let (lock, cvar) = &*SIGNAL;
        let mut woken = lock.lock().unwrap();
        while !*woken {
            woken = cvar.wait(woken).unwrap();
        }
        *woken = false;
    }
    fn unpark(&self) {
        let (lock, cvar) = &*SIGNAL;
        *lock.lock().unwrap() = true;
        cvar.notify_one();
    }
}

/// psync's default task queue, parking with [`ThreadPark`].
#[derive(Default)]
pub struct HostPool(psync::DefaultPool);

impl Pool for HostPool {
    type Park = ThreadPark;
    fn push(&self, task: Task) {
        self.0.push(task);
    }
    fn take_ready(&self) -> Option<Task> {
        self.0.take_ready()
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Deadlines waiting to fire, earliest first.
struct Timers {
    queue: Mutex<BinaryHeap<std::cmp::Reverse<(Instant, u64, WakerSlot)>>>,
    changed: Condvar,
}

/// A waker stored in the timer heap (ordered by its sequence number only).
struct WakerSlot(Waker);
impl PartialEq for WakerSlot {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for WakerSlot {}
impl PartialOrd for WakerSlot {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for WakerSlot {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

static TIMERS: LazyLock<&'static Timers> = LazyLock::new(|| {
    let timers: &'static Timers = Box::leak(Box::new(Timers {
        queue: Mutex::default(),
        changed: Condvar::new(),
    }));
    std::thread::spawn(move || {
        let mut queue = timers.queue.lock().unwrap();
        loop {
            let now = Instant::now();
            while queue.peek().is_some_and(|t| t.0.0 <= now) {
                queue.pop().expect("peeked").0.2.0.wake();
            }
            queue = match queue.peek().map(|t| t.0.0) {
                Some(next) => timers.changed.wait_timeout(queue, next - now).unwrap().0,
                None => timers.changed.wait(queue).unwrap(),
            };
        }
    });
    timers
});

/// Simulated I/O: completes `ms` milliseconds after it is first polled.
pub async fn sleep_ms(ms: u64) {
    let mut deadline = None;
    std::future::poll_fn(|cx| {
        let at = *deadline.get_or_insert_with(|| {
            let at = Instant::now() + Duration::from_millis(ms);
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            TIMERS.queue.lock().unwrap().push(std::cmp::Reverse((
                at,
                seq,
                WakerSlot(cx.waker().clone()),
            )));
            TIMERS.changed.notify_one();
            at
        });
        if Instant::now() >= at {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await
}

/// Run `future` to completion on a psync executor and time it.
pub fn time<T: 'static>(future: impl Future<Output = T> + 'static) -> (T, Duration) {
    let executor = Executor::new(HostPool::default());
    let out = Rc::new(RefCell::new(None));
    let slot = out.clone();
    let start = Instant::now();
    executor.spawn(1, async move {
        let value = pin!(future).await;
        *slot.borrow_mut() = Some(value);
    });
    loop {
        executor.run_until_stalled();
        if let Some(value) = out.borrow_mut().take() {
            return (value, start.elapsed());
        }
        ThreadPark.park();
    }
}
