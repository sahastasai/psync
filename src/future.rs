use alloc::boxed::Box;
use core::{future::Future, pin::Pin};

pub type BoxFuture<'t, T = ()> = Pin<Box<dyn Future<Output = T> + Send + 't>>;
pub type LocalBoxFuture<'t, T = ()> = Pin<Box<dyn Future<Output = T> + 't>>;

/// Returns `Pending` once (after waking itself), letting the executor run other ready tasks.
/// Works on any executor. `#[psync::graph(yield_every = N)]` inserts it into long loops.
pub fn yield_now() -> YieldNow {
    YieldNow(false)
}

/// The future returned by [`yield_now`].
pub struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> core::task::Poll<()> {
        if self.0 {
            return core::task::Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        core::task::Poll::Pending
    }
}
