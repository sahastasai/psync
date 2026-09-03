// Sairam.
use core::future::IntoFuture;
use crate::{BoxedFuture, LocalBoxedFuture};
use fxhash::FxHasher;
/// A pointer to the current [`TaskControlBlock`]. Used when kernel context switching. It is
/// currently considered unsafe to write to this value. Restrict usage to the scheduler 
/// thread, and ensure that nothing else mutates this.
#[no_mangle]
pub(crate) static mut CURRENT_TCB: *mut TaskControlBlock = core::ptr::null_mut();
/// A pointer to the next [`TaskControlBlock`]. Used when kernel context switching. See notes on
/// safety in the string for [`CURRENT_TCB`].
#[no_mangle]
pub(crate) static mut NEXT_TCB: *mut TaskControlBlock = core::ptr::null_mut();

/// The trait for a Task.
pub trait Task: IntoFuture + Sized {
    /// The result of this task.
    type Result;
    /// The [`async`] function that controls the running of this [`Task`].
    async fn run(&mut self) -> Self::Result;
    /// A synchronous function that gives this function's priority. This must call
    /// [`Pool::priority_trigger`] if it changes its own priority at any point.
    fn priority(self) -> u16;
    /// A synchronous function that allows other functions to change the priority of this function.
    /// If the priority change is approved within this function, it must call
    /// [`Pool::priority_trigger`].
    fn priority_suggestion(&mut self, identifier: u32, suggested_priority: u16) -> bool;
    /// A synchronous function that returns the fingerprint of this [`Task`]. This is
    /// useful in cases where this [`Task`] must be uniquely identified. Use the default
    /// implementation unless you have a really good reason not to do so.
    fn fingerprint(self) -> u64 {
        let mut hasher = FxHasher::default();
        hasher.write_usize(self.run as usize);
        hasher.write_usize(self.priority as usize);
        hasher.write_usize(self.fingerprint as usize);
        hasher.finish()
    }

}
/// Blocks that control tasks as wrappers for Futures for the sole purpose of
/// allowing context switching from the main thread (or main core in the case
/// of RP235x controllers).
#[repr(C)]
pub struct TaskControlBlock<T: Task> {
    stack_ptr: u32,
    task: T
}

/// Controls how a [`TaskControlBlock`] is cast into a [`LocalBoxedFuture`].
impl IntoFuture for TaskControlBlock<T: Task> {
    /// See [`Task::Result`]; the `Result`ant type of the [`Task`] being performed.
    type Output = T::Result;
    /// The returned `Future` type.  
    type IntoFuture = LocalBoxedFuture;

    /// A conversion from [`TaskControlBlock`] to [`LocalBoxedFuture`].
    fn into_future(self) -> Self::IntoFuture {
        self.task.into_future();
    }
}
