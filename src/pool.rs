// Sairam.
use crate::{Park, Task, TaskControlBlock, LocalBoxedFuture};

/// Represents the default [`Pool::Item`] element that should be used in [`Pool`]s.
struct DefaultItem(TaskControlBlock<&dyn Trait>, u16);
/// A trait that holds a series of tasks to be executed.
pub trait Pool {
    /// See [`Park`]. This handles the sleeping and waking of the executor.
    type Park: Park;
    /// The unit type for an Item (task, loosely speaking) in this [`Pool`]. 
    type Item = DefaultItem;
    /// Add a new [`Task`] to this [`Pool`].
    fn push(&mut self, task: LocalBoxedFuture<'static>);
    /// A method that allows [`Task`]s to inform the [`Pool`] when their priority changes.
    fn priority_trigger(&mut self, task_fingerprint: u64);
}

