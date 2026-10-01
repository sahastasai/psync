//! A cooperative, single-core async executor. Larger priorities run first.
#![no_std]

extern crate alloc;

mod executor;
mod future;
mod os;
mod park;
mod pool;
mod task;

pub use executor::Executor;
pub use future::{BoxFuture, LocalBoxFuture};
pub use park::{DefaultPark, Park};
pub use pool::{DefaultPool, Pool};
pub use task::{Task, TaskHandle};

#[cfg(all(feature = "rp235x", target_os = "none", target_arch = "arm"))]
#[global_allocator]
static HEAP: embedded_alloc::LlffHeap = embedded_alloc::LlffHeap::empty();

/// Initialize the firmware heap before creating an executor.
///
/// # Safety
/// Call exactly once, before any allocation, with no concurrent heap access.
/// The application's linker script must provide valid heap boundaries.
#[cfg(all(feature = "rp235x", target_os = "none", target_arch = "arm"))]
pub unsafe fn init_heap() {
    unsafe extern "C" {
        static mut _heap_start: u8;
        static mut _heap_end: u8;
    }
    let start = &raw mut _heap_start as usize;
    let end = &raw mut _heap_end as usize;
    unsafe { HEAP.init(start, end - start) };
}
