use crate::{DefaultPool, LocalBoxFuture, Park, Pool, Task, TaskHandle};
use alloc::{boxed::Box, rc::Rc, sync::Arc};
use core::future::Future;

/// Cooperative executor for local futures; keep it on one core.
/// Higher numbers run first. A continuously ready high-priority task can starve lower priorities.
pub struct Executor<P: Pool = DefaultPool> {
    pool: Rc<P>,
    park: Arc<P::Park>,
}
impl<P: Pool> Clone for Executor<P> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            park: self.park.clone(),
        }
    }
}
impl Default for Executor {
    fn default() -> Self {
        Self::new(DefaultPool::default())
    }
}
impl<P: Pool> Executor<P> {
    pub fn new(pool: P) -> Self {
        Self {
            pool: Rc::new(pool),
            park: Arc::new(P::Park::default()),
        }
    }
    pub fn spawn_future(&self, future: LocalBoxFuture<'static>) -> TaskHandle {
        self.spawn_future_with_priority(0, future)
    }
    pub fn spawn_boxed(&self, future: impl Future<Output = ()> + 'static) -> TaskHandle {
        self.spawn(0, future)
    }
    pub fn spawn(&self, priority: u16, future: impl Future<Output = ()> + 'static) -> TaskHandle {
        self.spawn_future_with_priority(priority, Box::pin(future))
    }
    pub fn spawn_future_with_priority(
        &self,
        priority: u16,
        future: LocalBoxFuture<'static>,
    ) -> TaskHandle {
        let (task, handle) = Task::new(future, priority, self.park.clone());
        self.pool.push(task);
        self.park.unpark();
        handle
    }
    /// Poll one ready task. Returns false when all remaining tasks are sleeping.
    pub fn poll_once(&self) -> bool {
        let Some(mut task) = self.pool.take_ready() else {
            return false;
        };
        if task.poll().is_pending() {
            self.pool.push(task);
        }
        true
    }
    /// Runs until no task is ready; self-waking tasks may keep this running forever.
    pub fn run_until_stalled(&self) {
        while self.poll_once() {}
    }
    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
    }
    /// Run forever, parking whenever no task is ready.
    pub fn run(&self) -> ! {
        loop {
            if !self.poll_once() {
                self.park.park();
            }
        }
    }
}
