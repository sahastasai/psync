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
