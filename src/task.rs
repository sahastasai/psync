// Sairam.
use core::future::IntoFuture;
/// A pointer to the current `TaskControlBlock`. Used when kernel context switching. It is
/// currently considered unsafe to write to this value. Restrict usage to the scheduler 
/// thread, and ensure that nothing else mutates this.
#[no_mangle]
pub(crate) static mut CURRENT_TCB: *mut TaskControlBlock = core::ptr::null_mut();
/// A pointer to the next `TaskControlBlock`. Used when kernel context switching. See notes on
/// safety in the string for `CURRENT_TCB`.
#[no_mangle]
pub(crate) static mut NEXT_TCB: *mut TaskControlBlock = core::ptr::null_mut();

/// The trait for a Task.
pub trait Task: IntoFuture + Sized {
    /// The result of this task.
    type Result;
    /// The `async` function that controls the running of this `Task`.
    async fn run() -> Self::Result;
    /// A synchronous function that gives this function's priority. This must call the
    /// `Executor`'s `change_priority` function. TODO implement `change_priority` and replace this
    /// docstring.
    fn priority() -> u16;
}
/// Blocks that control tasks as wrappers for Futures for the sole purpose of
/// allowing context switching from the main thread (or main core in the case
/// of RP235x controllers).
#[repr(C)]
pub struct TaskControlBlock<T: Task> {
    stack_ptr: u32,
    task: T
}

/// Controls how a `TaskControlBlock` is cast into a `Future`.
impl IntoFuture for TaskControlBlock<T: Task> {
    type Output = T::Result;
    type IntoFuture = Pin<Box<dyn Future<Output = T::Result>>>;

    fn into_future(self) -> Self::IntoFuture {
        self.task.into_future();
    }
}
