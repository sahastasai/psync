use psync::Executor;
use std::{cell::RefCell, future::poll_fn, rc::Rc, task::Poll};

#[test]
fn priority_and_fifo() {
    let executor = Executor::default();
    let log = Rc::new(RefCell::new(Vec::new()));
    for (priority, id) in [(1, 0), (8, 1), (8, 2), (3, 3)] {
        let log = log.clone();
        executor.spawn(priority, async move {
            log.borrow_mut().push(id);
        });
    }
    executor.run_until_stalled();
    assert_eq!(*log.borrow(), [1, 2, 3, 0]);
    assert!(executor.is_empty());
}

#[test]
fn self_waking_tasks_rotate_and_completed_tasks_stay_complete() {
    let executor = Executor::default();
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut handles = Vec::new();
    for id in 0..2 {
        let log = log.clone();
        let mut polls = 0;
        handles.push(executor.spawn(
            5,
            poll_fn(move |cx| {
                log.borrow_mut().push(id);
                polls += 1;
                cx.waker().wake_by_ref();
                if polls == 2 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }),
        ));
    }
    executor.run_until_stalled();
    assert_eq!(*log.borrow(), [0, 1, 0, 1]);
    for handle in handles {
        handle.wake();
    }
    assert!(!executor.poll_once());
    assert!(executor.is_empty());
}

#[test]
fn sleeping_high_priority_does_not_block_and_cross_thread_wake_works() {
    let executor = Executor::default();
    let saved = Rc::new(RefCell::new(None));
    let saved_task = saved.clone();
    let mut polls = 0;
    executor.spawn(
        100,
        poll_fn(move |cx| {
            polls += 1;
            if polls == 1 {
                *saved_task.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }),
    );
    executor.spawn(0, async {});
    assert!(executor.poll_once());
    assert!(executor.poll_once());
    assert!(!executor.poll_once());
    assert!(!executor.is_empty());
    let waker = saved.borrow_mut().take().unwrap();
    std::thread::spawn(move || waker.wake()).join().unwrap();
    assert!(executor.poll_once());
    assert!(executor.is_empty());
}

#[test]
fn changing_priority_and_spawning_during_poll() {
    let executor = Executor::default();
    let log = Rc::new(RefCell::new(Vec::new()));
    let low_log = log.clone();
    let low = executor.spawn(0, async move {
        low_log.borrow_mut().push(0);
    });
    let clone = executor.clone();
    let high_log = log.clone();
    executor.spawn(10, async move {
        high_log.borrow_mut().push(10);
        low.set_priority(30);
        clone.spawn(20, async move {
            high_log.borrow_mut().push(20);
        });
    });
    executor.run_until_stalled();
    assert_eq!(*log.borrow(), [10, 0, 20]);
}

#[test]
fn reprioritizing_sleeping_task_wakes_it() {
    let executor = Executor::default();
    let mut first = true;
    let handle = executor.spawn(
        0,
        poll_fn(move |_| {
            if first {
                first = false;
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }),
    );
    executor.run_until_stalled();
    handle.set_priority(u16::MAX);
    assert_eq!(handle.priority(), u16::MAX);
    assert!(executor.poll_once());
    assert!(executor.is_empty());
}
