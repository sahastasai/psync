pub(crate) fn park() {
    #[cfg(all(feature = "rp235x", target_os = "none", target_arch = "arm"))]
    unsafe {
        core::arch::asm!("wfe", options(nostack, preserves_flags))
    };
    #[cfg(not(all(feature = "rp235x", target_os = "none", target_arch = "arm")))]
    core::hint::spin_loop();
}

pub(crate) fn unpark() {
    #[cfg(all(feature = "rp235x", target_os = "none", target_arch = "arm"))]
    unsafe {
        core::arch::asm!("dsb", "sev", options(nostack, preserves_flags))
    };
}
