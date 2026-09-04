//! # psync
//! An `async` runtime for RP2350 (for now) with support for task priority and dependency
//! enumeration.
#![no_std]
#![no_main]
extern crate alloc;
use embedded_alloc::Heap;

#[global_allocator]
static HEAP: Heap = Heap::empty();

extern "C" {
    static mut _heap_start: u8;
    static mut _heap_end: u8;
}

pub fn init_heap() {
    unsafe {
        let start = &raw mut _heap_start as usize;
        let end = &raw mut _heap_end as usize;
        let size = end - start;
        HEAP.init(start, size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
