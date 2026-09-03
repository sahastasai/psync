//! # psync
//! An `async` runtime for RP2350 (for now) with support for task priority and dependency
//! enumeration.
#![no_std]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
