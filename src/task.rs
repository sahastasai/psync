use crate::{LocalBoxFuture, Park};
use alloc::{sync::Arc, task::Wake};
use core::{
    sync::atomic::{AtomicBool, AtomicU16, Ordering},
    task::{Context, Poll, Waker},
};

pub(crate) struct Signal {
    ready: AtomicBool,
    priority: AtomicU16,
    park: Arc<dyn ParkSignal>,
}
trait ParkSignal: Send + Sync {
    fn unpark(&self);
}
impl<P: Park> ParkSignal for P {
    fn unpark(&self) {
        Park::unpark(self);
    }
}
impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.ready.store(true, Ordering::Release);
        self.park.unpark();
    }
}

/// Changes priority and wakes a task. Handles never contain the task's local future.
#[derive(Clone)]
pub struct TaskHandle(Arc<Signal>);
impl TaskHandle {
    pub fn priority(&self) -> u16 {
        self.0.priority.load(Ordering::Acquire)
    }
    pub fn set_priority(&self, priority: u16) {
        self.0.priority.store(priority, Ordering::Release);
        self.wake();
    }
    pub fn wake(&self) {
        self.0.wake_by_ref();
    }
}

/// A scheduled future, owned by its executor's pool.
pub struct Task {
    future: LocalBoxFuture<'static>,
    signal: Arc<Signal>,
}
impl Task {
    pub(crate) fn new<P: Park>(
        future: LocalBoxFuture<'static>,
        priority: u16,
        park: Arc<P>,
    ) -> (Self, TaskHandle) {
        let signal = Arc::new(Signal {
            ready: AtomicBool::new(true),
            priority: AtomicU16::new(priority),
            park,
        });
        let handle = TaskHandle(signal.clone());
        (Self { future, signal }, handle)
    }
    pub fn is_ready(&self) -> bool {
        self.signal.ready.load(Ordering::Acquire)
    }
    pub fn priority(&self) -> u16 {
        self.signal.priority.load(Ordering::Acquire)
    }
    pub(crate) fn poll(&mut self) -> Poll<()> {
        // Clear before polling so a wake during poll is retained.
        self.signal.ready.store(false, Ordering::Release);
        let waker = Waker::from(self.signal.clone());
        self.future.as_mut().poll(&mut Context::from_waker(&waker))
    }
}
