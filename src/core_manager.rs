/// This file is for RP235x usage only.
use rp235x_hal::multicore::{Multicore, Stack};
use crate::Executor;
/// The default size of a stack on a single core.
const DEFAULT_STACK_SIZE: u32 = 65536;

/// Manages the cores being used in this instance.
struct Core1Manager {
    multicore: Multicore,
    stack: Stack<DEFAULT_STACK_SIZE>,
}
// TODO: Finish Core1Manager
impl Core1Manager {
    fn init() -> Self {
        Self {
            multicore: Multicore::new(/*have to add the params here by grabbing it*/),
            stack: Stack::new(),
        }
    }
}
