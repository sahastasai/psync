/// Parking must preserve an unpark notification delivered just before park.
pub trait Park: Default + Send + Sync + 'static {
    fn park(&self);
    fn unpark(&self);
}

/// Uses WFE/SEV on RP2350 ARM, and spins on other targets.
#[derive(Debug, Default)]
pub struct DefaultPark;
impl Park for DefaultPark {
    fn park(&self) {
        crate::os::park();
    }
    fn unpark(&self) {
        crate::os::unpark();
    }
}
