// Sairam.
use crate::TaskControlBlock;

#[no_mangle]
pub static mut CURRENT_TCB: *mut TaskControlBlock = core::ptr::null_mut();

#[no_mangle]
pub static mut NEXT_TCB: *mut TaskControlBlock = core::ptr::null_mut();

/// Handles context switching. Activated by setting PendSV bit using
/// [`cortex_m::peripherals::SCB`](https://docs.rs/cortex-m/latest/cortex_m/peripheral/struct.SCB.html)
/// and is not mangled by the compiler.
#[no_mangle]
#[naked]
pub unsafe extern "C" fn PendSV() {
    asm!(
        "cpsid i", // 1. Guard scheduler critical section

        // 2. Extract current stack pointer and map to r0
        "mrs r0, psp",

        // 3. Check EXC_RETURN (lr) bit 4 to see if FPU state exists
        // tst performs a bitwise AND: if bit 4 is 0, the Z-flag is set.
        "tst lr, #0x10",
        "it eq",
        // vstmdbeq pushes callee registers S16-S31 onto r0 if Z-flag is set (eq)
        "vstmdbeq r0!, {s16-s31}",

        // 4. Save manual core registers
        "stmdb r0!, {r4-r11}",

        // 5. Store r0 back inside the current task's TCB
        "ldr r1, =CURRENT_TCB",
        "ldr r1, [r1]",
        "cmp r1, #0",
        "beq 1f",
        "str r0, [r1]",

        "1:",
        // 6. Transition pointers: CURRENT_TCB = NEXT_TCB
        "ldr r1, =NEXT_TCB",
        "ldr r2, [r1]",
        "ldr r3, =CURRENT_TCB",
        "str r2, [r3]",

        // 7. Extract the top-of-stack from the incoming task
        "ldr r0, [r2]",

        // 8. Restore manual core registers for incoming task
        "ldmia r0!, {r4-r11}",

        // 9. Re-check the incoming task's stack payload requirements
        // We look at the inbound task's saved context structure.
        // We extract the EXC_RETURN pattern which was saved alongside the task,
        // or check the global execution path. Assuming standard tasking:
        "tst lr, #0x10",
        "it eq",
        // vldmiaeq pops callee registers S16-S31 from r0 if incoming context expects it
        "vldmiaeq r0!, {s16-s31}",

        // 10. Update the CPU's active PSP to the new frame
        "msr psp, r0",
        "cpsie i",

        // 11. Exit Exception. 
        // The hardware checks EXC_RETURN to pop S0-S15 and FPSCR automatically.
        "bx lr",
        options(noreturn)
    );
}
