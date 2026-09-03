use alloc::sync::Arc;
use embedded_hal::digital::{InputPin, OutputPin};
use rp235x_hal::{self as hal, clocks::{init_clocks_and_plls, ClocksManager}, gpio::Pins, gpio::Pin, watchdog::Watchdog, Sio,};
use rp235x_hal::gpio::{DynFunction, DynSioConfig, InputOverride, SioInput, SioOutput, AnyPin, Function, ValidFunction, SioConfig, PinId, PullType, FunctionSio};
use cortex_m::interrupt::{self, Mutex};
use once_cell::race::OnceBox;
use alloc::boxed::Box;
// TODO: Rustdoc comments across this file.
// TODO: Write a version of this file without using the heap?
// Updated to be fully concurrent using alloc Arcs, cortex_m Mutexes, and once_cell racing boxes
// Inits
const XOSC_CRYSTAL_FREQ: u32 = 12_000_000;
static sio: OnceBox<Arc<Mutex<Sio>>> = OnceBox::new();
static watchdog: OnceBox<Arc<Mutex<Watchdog>>> = OnceBox::new();
static clocks: OnceBox<Arc<Mutex<ClocksManager>>> = OnceBox::new();
// System initer helper method
fn init_systems() -> (Watchdog, hal::gpio::Pins, ClocksManager) {
// Inits
    let mut peripherals = hal::pac::Peripherals::take().unwrap();
    let mut watchdogx = Watchdog::new(peripherals.WATCHDOG);
    let mut clocksx = init_clocks_and_plls(XOSC_CRYSTAL_FREQ, peripherals.XOSC, peripherals.CLOCKS, peripherals.PLL_SYS, peripherals.PLL_USB, &mut peripherals.RESETS, &mut watchdogx).ok().unwrap();
    let mut pac = hal::pac::Peripherals::take().unwrap();
    let siox = Sio::new(peripherals.SIO);
    let pinsx = rp235x_hal::gpio::Pins::new(peripherals.IO_BANK0, peripherals.PADS_BANK0, siox.gpio_bank0, &mut peripherals.RESETS);
    (watchdogx, pinsx, clocksx)
}
pub enum VariableSelect {
    PINS, WATCHDOG, CLOCKS
}

pub enum VariableFulfilled<'a> {
    PINS(hal::gpio::Pins), WATCHDOG(&'a Arc<Mutex<Watchdog>>), CLOCKS(&'a Arc<Mutex<ClocksManager>>)
}
pub fn system_initer() {
    let a = init_systems();
    watchdog.get_or_init(|| {
        Box::new(Arc::new(Mutex::new(a.0)))
    });
    clocks.get_or_init(|| {
        Box::new(Arc::new(Mutex::new(a.2)))
    });
}
// Panics without a proper call to system_initer prior
pub fn get_variable(v: VariableSelect) -> VariableFulfilled<'static> {
    match(v) {
        VariableSelect::PINS => { return VariableFulfilled::PINS(init_systems().1) },
        VariableSelect::WATCHDOG => {return VariableFulfilled::WATCHDOG(watchdog.get().unwrap())},
        VariableSelect::CLOCKS => {return VariableFulfilled::CLOCKS(clocks.get().unwrap())}
    }
}

