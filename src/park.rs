// Sairam.

/// A trait for the implementation of parking and unparking threads. See [`pasts::Park`].
pub trait Park: Default + Send + Sync + 'static {
    /// Parks a thread (that is, puts it to sleep to save power and CPU compute).
    fn park(&self);
    /// Wakes a thread from its state of being parked.
    fn unpark(&self);
}
