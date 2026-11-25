//! A multi-threaded, priority-aware executor (feature `std`).
//!
//! [`MultiExecutor`] runs `Send` futures on a fixed set of worker threads that share one ready
//! queue:
//!
//! - **Priorities.** A worker always takes the ready task with the highest priority; tasks of
//!   equal priority run in the order they became ready (FIFO), as on the single-core
//!   [`Executor`](crate::Executor). Priorities can change while a task waits; it is re-queued
//!   under its new priority without losing its place among equals.
//! - **Fairness.** Strict priorities starve: a busy high-priority task can keep lower ones from
//!   ever running. With [`MultiExecutor::with_fairness`]`(n)`, every `n`-th task a worker takes is
//!   the one that has been ready the longest, whatever its priority. A ready task then waits at
//!   most about `n` times the number of tasks ahead of it in age order.
//! - **Bounded cooperation.** A task that never awaits cannot be interrupted by any cooperative
//!   executor. `#[psync::graph(yield_every = N)]` inserts [`yield_now`](crate::yield_now) every
//!   `N` loop iterations of an async function, so long loops give the scheduler a chance to run
//!   higher-priority work.
//!
//! Wakes are never lost: a task woken while it is being polled is re-queued as soon as the poll
//! returns (the usual idle / queued / running / notified state machine).

extern crate std;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    task::Wake,
    vec::Vec,
};
use core::{
    cmp::Reverse,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;

type Job = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

const IDLE: u8 = 0;
const QUEUED: u8 = 1;
const RUNNING: u8 = 2;
const NOTIFIED: u8 = 3;
const DONE: u8 = 4;

struct Cell {
    future: Mutex<Option<Job>>,
    state: AtomicU8,
    priority: AtomicU16,
    /// The queue position (FIFO sequence number) while queued.
    seq: AtomicU64,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Queue {
    /// Ready tasks by priority, then age.
    by_priority: BTreeSet<(Reverse<u16>, u64)>,
    /// The same tasks by age, with the priority they were queued under.
    by_age: BTreeMap<u64, (u16, Arc<Cell>)>,
    next_seq: u64,
    picks: u64,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    idle: Condvar,
    live: AtomicUsize,
    shutdown: AtomicBool,
    fairness: Option<u64>,
}

impl Shared {
    fn enqueue(&self, cell: Arc<Cell>) {
        let mut q = self.queue.lock().expect("queue");
        let seq = q.next_seq;
        q.next_seq += 1;
        let priority = cell.priority.load(Ordering::Acquire);
        cell.seq.store(seq, Ordering::Release);
        q.by_priority.insert((Reverse(priority), seq));
        q.by_age.insert(seq, (priority, cell));
        drop(q);
        self.ready.notify_one();
    }

    /// The next task to run: the highest priority, or every `fairness`-th time the oldest.
    fn take(&self) -> Option<Arc<Cell>> {
        let mut q = self.queue.lock().expect("queue");
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return None;
            }
            q.picks += 1;
            let oldest = self.fairness.is_some_and(|n| q.picks.is_multiple_of(n));
            let seq = if oldest {
                q.by_age.keys().next().copied()
            } else {
                q.by_priority.iter().next().map(|(_, s)| *s)
            };
            if let Some(seq) = seq {
                let (priority, cell) = q.by_age.remove(&seq).expect("queued task");
                q.by_priority.remove(&(Reverse(priority), seq));
                return Some(cell);
            }
            q.picks -= 1;
            q = self.ready.wait(q).expect("queue");
        }
    }

    /// Re-key a queued task after a priority change (it keeps its age).
    fn requeue(&self, cell: &Arc<Cell>) {
        let mut q = self.queue.lock().expect("queue");
        let seq = cell.seq.load(Ordering::Acquire);
        if let Some((old, c)) = q.by_age.get(&seq).map(|(p, c)| (*p, c.clone()))
            && Arc::ptr_eq(&c, cell)
        {
            let new = cell.priority.load(Ordering::Acquire);
            q.by_priority.remove(&(Reverse(old), seq));
            q.by_priority.insert((Reverse(new), seq));
            q.by_age.insert(seq, (new, c));
        }
    }
}

struct CellWaker(Arc<Cell>);
impl Wake for CellWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        wake(&self.0);
    }
}

fn wake(cell: &Arc<Cell>) {
    let mut state = cell.state.load(Ordering::Acquire);
    loop {
        let next = match state {
            IDLE => QUEUED,
            RUNNING => NOTIFIED,
            _ => return,
        };
        match cell
            .state
            .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                if next == QUEUED {
                    cell.shared.enqueue(cell.clone());
                }
                return;
            }
            Err(actual) => state = actual,
        }
    }
}

fn run(cell: Arc<Cell>) {
    cell.state.store(RUNNING, Ordering::Release);
    let waker = Waker::from(Arc::new(CellWaker(cell.clone())));
    let mut slot = cell.future.lock().expect("future");
    let Some(future) = slot.as_mut() else {
        return;
    };
    match future.as_mut().poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(()) => {
            *slot = None;
            drop(slot);
            cell.state.store(DONE, Ordering::Release);
            let shared = &cell.shared;
            if shared.live.fetch_sub(1, Ordering::AcqRel) == 1 {
                let _q = shared.queue.lock().expect("queue");
                shared.idle.notify_all();
            }
        }
        Poll::Pending => {
            drop(slot);
            // Woken during the poll: run again.
            if cell
                .state
                .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                cell.state.store(QUEUED, Ordering::Release);
                cell.shared.enqueue(cell.clone());
            }
        }
    }
}

/// A priority-aware executor running `Send` futures on worker threads.
pub struct MultiExecutor {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

/// Changes the priority of, wakes, or checks a task of a [`MultiExecutor`].
#[derive(Clone)]
pub struct MultiHandle(Arc<Cell>);

impl MultiHandle {
    pub fn priority(&self) -> u16 {
        self.0.priority.load(Ordering::Acquire)
    }
    /// Change the priority; a queued task moves to its new place right away.
    pub fn set_priority(&self, priority: u16) {
        self.0.priority.store(priority, Ordering::Release);
        if self.0.state.load(Ordering::Acquire) == QUEUED {
            self.0.shared.requeue(&self.0);
        }
    }
    pub fn wake(&self) {
        wake(&self.0);
    }
    pub fn is_finished(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == DONE
    }
}

impl MultiExecutor {
    /// `workers` threads with strict priorities (FIFO among equals).
    pub fn new(workers: usize) -> Self {
        Self::build(workers, None)
    }

    /// `workers` threads; every `every`-th pick takes the longest-waiting ready task.
    pub fn with_fairness(workers: usize, every: u32) -> Self {
        Self::build(workers, Some(u64::from(every.max(1))))
    }

    fn build(workers: usize, fairness: Option<u64>) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            ready: Condvar::new(),
            idle: Condvar::new(),
            live: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            fairness,
        });
        let workers = (0..workers.max(1))
            .map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name(alloc::format!("psync-worker-{i}"))
                    .spawn(move || {
                        while let Some(cell) = shared.take() {
                            run(cell);
                        }
                    })
                    .expect("spawn worker")
            })
            .collect();
        Self { shared, workers }
    }

    /// Run `future` at `priority` (larger first).
    pub fn spawn(
        &self,
        priority: u16,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> MultiHandle {
        let cell = Arc::new(Cell {
            future: Mutex::new(Some(Box::pin(future))),
            state: AtomicU8::new(QUEUED),
            priority: AtomicU16::new(priority),
            seq: AtomicU64::new(0),
            shared: self.shared.clone(),
        });
        self.shared.live.fetch_add(1, Ordering::AcqRel);
        self.shared.enqueue(cell.clone());
        MultiHandle(cell)
    }

    /// Run `future` at `priority` and wait for its output on this thread.
    pub fn block_on<T: Send + 'static>(
        &self,
        priority: u16,
        future: impl Future<Output = T> + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.spawn(priority, async move {
            let _ = tx.send(future.await);
        });
        rx.recv().expect("the task panicked")
    }

    /// Wait until every spawned task has finished.
    pub fn wait_idle(&self) {
        let mut q = self.shared.queue.lock().expect("queue");
        while self.shared.live.load(Ordering::Acquire) > 0 {
            q = self.shared.idle.wait(q).expect("queue");
        }
    }
}

impl Drop for MultiExecutor {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        {
            let _q = self.shared.queue.lock().expect("queue");
            self.shared.ready.notify_all();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
