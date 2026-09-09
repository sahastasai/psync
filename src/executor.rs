// Sairam.
use alloc::sync::Arc;
use crate::{LocalBoxFuture, Pool};

/// Executes tasks from the provided [`Pool`].
pub struct Executor<P: Pool>(Arc<P>);

impl<P: Pool> Clone for Executor<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0));
    }
}

impl<P: Pool> Executor<P> {
    /// Create a new [`Executor`] from [`Pool`].
    pub fn new(pool: P) -> Self {
        Self(Arc::new(pool));
    }
}
