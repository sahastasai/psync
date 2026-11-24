//! psync: automatic concurrency for async Rust, and a small priority executor.
//!
//! - [`graph`] (`#[psync::graph]`) rewrites an async function so that awaits which do not depend
//!   on each other run concurrently, with results, errors, early exits and writes to files and
//!   services kept as in the sequential code. The hints [`seq`], [`fresh`], [`independent`] and
//!   [`speculate`] tell it what tokens cannot show. The generated code needs only `core` and
//!   `alloc` (loop fission also uses `std`'s `Vec`) and runs on any executor.
//! - [`Executor`] is a cooperative, single-core, `no_std` executor: larger priorities run first,
//!   equal priorities rotate FIFO, and sleeping tasks are polled only after a wake.
//!
//! The psync book (`book/` in the repository) covers both in depth.
#![no_std]

extern crate alloc;

#[doc(hidden)]
#[path = "graph_rt.rs"]
pub mod __private;
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
pub use psync_macros::{fresh, graph, independent, seq, speculate};
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
