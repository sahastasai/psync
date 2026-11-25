//! The example of psync's `verify/Footprint.lean`, in Rust: a recursive callee that takes two
//! places by reference, called on two disjoint fields of one struct.

pub struct S {
    pub a: u64,
    pub b: u64,
    pub c: u64,
    pub d: u64,
}

/// `if *x != 0 { *y += 1; *x -= 1; drain(x, y) }`
pub fn drain(x: &mut u64, y: &mut u64) {
    if *x != 0 {
        *y = y.wrapping_add(1);
        *x -= 1;
        drain(x, y)
    }
}

/// The two statements psync would run in one level: their footprints are `s.a, s.b` and `s.c, s.d`.
pub fn callers(s: &mut S) {
    drain(&mut s.a, &mut s.b);
    drain(&mut s.c, &mut s.d);
}

/// The same calls in the other order.
pub fn callers_swapped(s: &mut S) {
    drain(&mut s.c, &mut s.d);
    drain(&mut s.a, &mut s.b);
}
