//! # psync
//! An `async` runtime for RP2350 (for now) with support for task priority and dependency
//! enumeration.
#![no_std]
#![no_main]

#[cfg(feature = "rp235x")]
extern crate alloc;
#[cfg(feature = "rp235x")]
use embedded_alloc::LlffHeap as Heap;
    
#[cfg(feature = "rp235x")]    
#[global_allocator]
static HEAP: Heap = Heap::empty();

#[cfg(feature = "rp235x")]
unsafe extern "C" {
    static mut _heap_start: u8;
    static mut _heap_end: u8;
}

#[cfg(feature = "rp235x")]
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
