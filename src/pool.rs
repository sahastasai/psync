use crate::{DefaultPark, Park, Task};
use alloc::collections::VecDeque;
use core::cell::RefCell;

/// A local task queue. `take_ready` must remove the highest-priority ready task.
pub trait Pool {
    type Park: Park;
    fn push(&self, task: Task);
    fn take_ready(&self) -> Option<Task>;
    fn is_empty(&self) -> bool;
}

#[derive(Default)]
pub struct DefaultPool {
    tasks: RefCell<VecDeque<Task>>,
}
impl Pool for DefaultPool {
    type Park = DefaultPark;
    fn push(&self, task: Task) {
        self.tasks.borrow_mut().push_back(task);
    }
    fn take_ready(&self) -> Option<Task> {
        let mut tasks = self.tasks.borrow_mut();
        let mut best = None;
        for (index, task) in tasks.iter().enumerate() {
            if task.is_ready() && best.is_none_or(|(_, priority)| task.priority() > priority) {
                best = Some((index, task.priority()));
            }
        }
        best.and_then(|(index, _)| tasks.remove(index))
    }
    fn is_empty(&self) -> bool {
        self.tasks.borrow().is_empty()
    }
}
