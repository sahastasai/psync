//! Phase II's runtime: run the chains of one level of synchronous code on threads.
//!
//! [`par_run`] runs its first job on the calling thread and the others on a persistent pool of
//! parked workers, and returns only when every job has finished. That is what lets the jobs borrow
//! from the caller's stack frame, as `std::thread::scope` does, at about a tenth of its cost per
//! job (a parked worker is woken; no thread is created). The pool grows when every worker is busy,
//! so a job that blocks on I/O never waits for a worker that is itself waiting for it, and past
//! [`MAX_WORKERS`] a job runs on the calling thread instead.
//!
//! The pool is separate from rayon's on purpose: its workers block on I/O, and blocking in rayon's
//! CPU-sized pool would starve everything else that uses rayon.

extern crate std;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

/// Past this many workers, jobs run on the thread that asked for them.
pub const MAX_WORKERS: usize = 256;

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Pool {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    idle: AtomicUsize,
    workers: AtomicUsize,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool {
        queue: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
        idle: AtomicUsize::new(0),
        workers: AtomicUsize::new(0),
    })
}

impl Pool {
    /// Queue `job`, waking or starting a worker for it. Gives the job back if the pool is full.
    fn submit(&'static self, job: Job) -> Result<(), Job> {
        let mut queue = self.queue.lock().unwrap();
        // Every queued job needs a worker of its own: a worker may block until a later job runs.
        if self.idle.load(Ordering::Acquire) <= queue.len() {
            if self.workers.load(Ordering::Acquire) >= MAX_WORKERS {
                return Err(job);
            }
            self.workers.fetch_add(1, Ordering::AcqRel);
            std::thread::Builder::new()
                .name("psync-par".into())
                .spawn(move || self.work())
                .expect("psync: could not start a worker thread");
        }
        queue.push_back(job);
        drop(queue);
        self.ready.notify_one();
        Ok(())
    }

    fn work(&'static self) {
        let mut queue = self.queue.lock().unwrap();
        loop {
            if let Some(job) = queue.pop_front() {
                drop(queue);
                job();
                queue = self.queue.lock().unwrap();
            } else {
                self.idle.fetch_add(1, Ordering::AcqRel);
                queue = self.ready.wait(queue).unwrap();
                self.idle.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

/// Counts unfinished jobs; [`Latch::wait`] returns when there are none.
struct Latch {
    left: Mutex<usize>,
    done: Condvar,
}

impl Latch {
    fn finish(&self) {
        let mut left = self.left.lock().unwrap();
        *left -= 1;
        if *left == 0 {
            self.done.notify_all();
        }
    }
    fn wait(&self) {
        let mut left = self.left.lock().unwrap();
        while *left > 0 {
            left = self.done.wait(left).unwrap();
        }
    }
}

/// Waits for the pool's jobs even if the caller's own job panics, so no job outlives the
/// borrows it was given.
struct WaitOnDrop<'a>(&'a Latch);
impl Drop for WaitOnDrop<'_> {
    fn drop(&mut self) {
        self.0.wait();
    }
}

/// A pointer that may cross threads: [`par_run`] guarantees what it points to outlives the job.
struct SendPtr<T: ?Sized>(*mut T);
unsafe impl<T: ?Sized> Send for SendPtr<T> {}

/// Run every job, the first on this thread and the rest on the pool, and return when all have
/// finished. A job that panics on a worker is caught there, and the panic is resumed here after
/// every job has finished (psync's generated jobs catch their own panics, to replay them in
/// program order; this is the backstop).
pub fn par_run(jobs: &mut [&mut (dyn FnMut() + Send)]) {
    let Some((first, rest)) = jobs.split_first_mut() else {
        return;
    };
    if rest.is_empty() {
        first();
        return;
    }
    let latch = Latch {
        left: Mutex::new(rest.len()),
        done: Condvar::new(),
    };
    let panicked: Mutex<Option<Box<dyn core::any::Any + Send>>> = Mutex::new(None);
    {
        let _wait = WaitOnDrop(&latch);
        let mut inline = alloc::vec::Vec::new();
        for job in rest.iter_mut() {
            // SAFETY: `_wait` blocks until every submitted job has called `finish`, so the job,
            // the latch and `panicked` outlive every use of these pointers, even if this thread
            // panics while running its own job.
            let job: *mut (dyn FnMut() + Send + '_) = &mut **job;
            let job: SendPtr<dyn FnMut() + Send + 'static> = SendPtr(unsafe {
                core::mem::transmute::<
                    *mut (dyn FnMut() + Send + '_),
                    *mut (dyn FnMut() + Send + 'static),
                >(job)
            });
            let latch_ptr = SendPtr(&latch as *const Latch as *mut Latch);
            let panicked_ptr = SendPtr(&panicked as *const _ as *mut Mutex<Option<_>>);
            let run: Job = Box::new(move || {
                let (job, latch_ptr, panicked_ptr) = (job, latch_ptr, panicked_ptr);
                let result = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| unsafe {
                    (*job.0)()
                }));
                unsafe {
                    if let Err(p) = result {
                        (*panicked_ptr.0).lock().unwrap().get_or_insert(p);
                    }
                    (*latch_ptr.0).finish();
                }
            });
            if let Err(run) = pool().submit(run) {
                inline.push(run);
            }
        }
        first();
        for run in inline {
            run();
        }
    }
    if let Some(p) = panicked.into_inner().unwrap() {
        std::panic::resume_unwind(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn jobs_borrow_and_overlap() {
        let (mut a, mut b, mut c) = (0, 0, 0);
        let start = Instant::now();
        par_run(&mut [
            &mut || {
                std::thread::sleep(Duration::from_millis(50));
                a = 1
            },
            &mut || {
                std::thread::sleep(Duration::from_millis(50));
                b = 2
            },
            &mut || {
                std::thread::sleep(Duration::from_millis(50));
                c = 3
            },
        ]);
        assert_eq!((a, b, c), (1, 2, 3));
        assert!(
            start.elapsed() < Duration::from_millis(120),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn nested_jobs_do_not_deadlock() {
        let mut total = 0;
        let mut outer: alloc::vec::Vec<usize> = alloc::vec![0; 8];
        {
            let mut jobs: alloc::vec::Vec<alloc::boxed::Box<dyn FnMut() + Send + '_>> = outer
                .iter_mut()
                .map(|slot| {
                    alloc::boxed::Box::new(move || {
                        let (mut x, mut y) = (0, 0);
                        par_run(&mut [&mut || x = 1, &mut || y = 1]);
                        *slot = x + y;
                    }) as alloc::boxed::Box<dyn FnMut() + Send>
                })
                .collect();
            let mut refs: alloc::vec::Vec<&mut (dyn FnMut() + Send)> =
                jobs.iter_mut().map(|j| &mut **j as _).collect();
            par_run(&mut refs);
        }
        for v in outer {
            total += v;
        }
        assert_eq!(total, 16);
    }

    #[test]
    fn a_worker_panic_is_resumed_after_all_jobs() {
        let mut finished = false;
        let r = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
            par_run(&mut [
                &mut || {
                    std::thread::sleep(Duration::from_millis(20));
                    finished = true
                },
                &mut || panic!("job"),
            ]);
        }));
        assert!(r.is_err());
        assert!(finished);
    }
}
