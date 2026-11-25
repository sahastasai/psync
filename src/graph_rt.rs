//! Runtime support for `#[psync::graph]` expansions. Not a public API: the macro's
//! generated code is the only intended caller, and these items may change freely.

use alloc::boxed::Box;
/// For generated code, which cannot name `alloc` itself.
pub use alloc::vec::Vec;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// One branch of a concurrent join: a boxed future, its output, or nothing (taken).
pub enum MaybeDone<F: Future + Unpin> {
    Pending(F),
    Done(F::Output),
    Taken,
}

/// Box a future so a join can poll it without unsafe pin projection.
pub fn maybe_done<F: Future>(future: F) -> MaybeDone<Pin<Box<F>>> {
    MaybeDone::Pending(Box::pin(future))
}

impl<F: Future + Unpin> MaybeDone<F> {
    /// Poll if still pending. Returns true once an output is available.
    pub fn poll_step(&mut self, cx: &mut Context<'_>) -> bool {
        if let MaybeDone::Pending(future) = self {
            match Pin::new(future).poll(cx) {
                Poll::Ready(output) => *self = MaybeDone::Done(output),
                Poll::Pending => return false,
            }
        }
        true
    }
    /// Take the output. Panics unless `poll_step` returned true and nothing took it yet.
    pub fn take(&mut self) -> F::Output {
        match core::mem::replace(self, MaybeDone::Taken) {
            MaybeDone::Done(output) => output,
            _ => panic!("psync::graph: output taken before completion"),
        }
    }
}

/// A synchronous chain's job, tagged for `cargo psync check` as [`maybe_done_at`] tags a future.
/// It returns the closure unchanged.
#[inline(always)]
pub fn job_at<
    const LEVEL: usize,
    const ORDER: usize,
    const LAST: usize,
    const EXITS: bool,
    const EXIT_AT: usize,
    const SPEC: bool,
    const INDEP: bool,
    F,
>(
    job: F,
) -> F {
    job
}

/// [`maybe_done`], tagged for `cargo psync check`: the chain belongs to level `LEVEL` of the
/// enclosing function, runs statements `ORDER` to `LAST` (of its region), may exit early (`EXITS`,
/// first at statement `EXIT_AT`), and carries the
/// `speculate` (`SPEC`) and `independent` (`INDEP`) hints. The tags cost nothing at run time.
#[inline(always)]
pub fn maybe_done_at<
    const LEVEL: usize,
    const ORDER: usize,
    const LAST: usize,
    const EXITS: bool,
    const EXIT_AT: usize,
    const SPEC: bool,
    const INDEP: bool,
    F: Future,
>(
    future: F,
) -> MaybeDone<Pin<Box<F>>> {
    maybe_done(future)
}

/// Run the futures yielded by `iter`, at most `bound` at a time, returning outputs in input order.
pub fn join_all_bounded<I>(iter: I, bound: usize) -> JoinAllBounded<I>
where
    I: Iterator,
    I::Item: Future,
{
    JoinAllBounded {
        iter: Some(Box::new(iter)),
        active: Vec::new(),
        results: Vec::new(),
        bound: bound.max(1),
    }
}

pub struct JoinAllBounded<I: Iterator>
where
    I::Item: Future,
{
    // Boxed: the iterator is never pinned, so it need not be `Unpin`.
    iter: Option<Box<I>>,
    active: Vec<(usize, Pin<Box<I::Item>>)>,
    results: Vec<Option<<I::Item as Future>::Output>>,
    bound: usize,
}

// Futures are boxed and outputs are never pinned, so moving the combinator is fine.
impl<I: Iterator> Unpin for JoinAllBounded<I> where I::Item: Future {}

impl<I> Future for JoinAllBounded<I>
where
    I: Iterator,
    I::Item: Future,
{
    type Output = Vec<<I::Item as Future>::Output>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            // Start new items up to the bound, so each starts as soon as a slot frees.
            while this.active.len() < this.bound {
                let Some(next) = this.iter.as_mut().and_then(Iterator::next) else {
                    this.iter = None;
                    break;
                };
                this.active.push((this.results.len(), Box::pin(next)));
                this.results.push(None);
            }
            let before = this.active.len();
            let results = &mut this.results;
            this.active
                .retain_mut(|(index, future)| match future.as_mut().poll(cx) {
                    Poll::Ready(output) => {
                        results[*index] = Some(output);
                        false
                    }
                    Poll::Pending => true,
                });
            if this.active.is_empty() && this.iter.is_none() {
                let results = core::mem::take(&mut this.results);
                return Poll::Ready(results.into_iter().map(|r| r.expect("completed")).collect());
            }
            // Nothing finished, so nothing new can start: wait for a wake.
            if this.active.len() == before {
                return Poll::Pending;
            }
        }
    }
}

/// `?`-style split of a `Result` or `Option` into its value or a residual to rethrow later.
pub trait Branch {
    type Output;
    type Residual: Rethrow;
    fn branch(self) -> Result<Self::Output, Self::Residual>;
}

/// Re-raise a residual through the caller's own `?`, so `From` conversions match the original.
pub trait Rethrow {
    type Carrier;
    fn rethrow(self) -> Self::Carrier;
}

pub struct ResultResidual<E>(pub E);
pub struct OptionResidual;

impl<T, E> Branch for Result<T, E> {
    type Output = T;
    type Residual = ResultResidual<E>;
    fn branch(self) -> Result<T, ResultResidual<E>> {
        self.map_err(ResultResidual)
    }
}
impl<T> Branch for Option<T> {
    type Output = T;
    type Residual = OptionResidual;
    fn branch(self) -> Result<T, OptionResidual> {
        self.ok_or(OptionResidual)
    }
}
impl<E> Rethrow for ResultResidual<E> {
    type Carrier = Result<(), E>;
    fn rethrow(self) -> Result<(), E> {
        Err(self.0)
    }
}
impl Rethrow for OptionResidual {
    type Carrier = Option<()>;
    fn rethrow(self) -> Option<()> {
        None
    }
}

/// Returns `f` unchanged. Passing the async closure through this function gives its parameter
/// the item type of `iter` (a closure defined on its own cannot have it inferred from later use).
pub fn over<I, F, T>(_iter: &I, f: F) -> F
where
    I: IntoIterator,
    F: AsyncFn(I::Item) -> T,
{
    f
}

/// [`over`], tagged for `cargo psync check`: the prefetched prefix of loop `LOOP`, in a body that
/// may end the loop early (`TERMINATES`), with the `speculate` and `independent` hints.
#[inline(always)]
pub fn over_at<
    const LOOP: usize,
    const TERMINATES: bool,
    const SPEC: bool,
    const INDEP: bool,
    I,
    F,
    T,
>(
    iter: &I,
    f: F,
) -> F
where
    I: IntoIterator,
    F: AsyncFn(I::Item) -> T,
{
    over(iter, f)
}

/// For `#[psync::graph(yield_every = N)]`: true on every `N`-th call from one loop (`ticks` is a
/// counter static to that loop), when the loop should yield to the executor.
#[inline(always)]
pub fn tick(ticks: &core::sync::atomic::AtomicU32, every: u32) -> bool {
    ticks.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % every == every - 1
}

pub use crate::future::yield_now;
#[cfg(feature = "std")]
pub use crate::par::par_run;

/// Orders two chains of one join at run time: the later one waits for the earlier one's end when
/// the files they touch turn out to overlap (the macro could not tell them apart from tokens).
/// All chains of a join are polled by one task, so one waker slot suffices; the lock is never
/// contended, and the gate is `Sync` so the function's future stays `Send`.
pub struct Gate {
    open: core::sync::atomic::AtomicBool,
    lock: core::sync::atomic::AtomicBool,
    waker: core::cell::UnsafeCell<Option<core::task::Waker>>,
}

// SAFETY: `waker` is only touched while `lock` is held.
unsafe impl Sync for Gate {}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub const fn new() -> Self {
        Self {
            open: core::sync::atomic::AtomicBool::new(false),
            lock: core::sync::atomic::AtomicBool::new(false),
            waker: core::cell::UnsafeCell::new(None),
        }
    }

    fn with_waker<R>(&self, f: impl FnOnce(&mut Option<core::task::Waker>) -> R) -> R {
        use core::sync::atomic::Ordering;
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the lock is held.
        let r = f(unsafe { &mut *self.waker.get() });
        self.lock.store(false, Ordering::Release);
        r
    }

    /// The earlier chain has ended.
    pub fn open(&self) {
        self.open.store(true, core::sync::atomic::Ordering::Release);
        if let Some(w) = self.with_waker(Option::take) {
            w.wake();
        }
    }

    /// Wait until [`Gate::open`].
    pub async fn wait(&self) {
        core::future::poll_fn(|cx| {
            if self.open.load(core::sync::atomic::Ordering::Acquire) {
                return Poll::Ready(());
            }
            self.with_waker(|w| *w = Some(cx.waker().clone()));
            // Opened between the check and the store: do not miss it.
            if self.open.load(core::sync::atomic::Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}
