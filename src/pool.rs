// Sairam.
use crate::{Park, LocalBoxedFuture};

pub trait Pool {
    /// See [`Park`]. This handles the sleeping and waking of the executor.
    type Park: Park;
    /// Add a new [`Task`] to this [`Pool`].
    fn push(&self, task: LocalBoxedFuture<'static>);
}

