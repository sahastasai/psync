use alloc::boxed::Box;
use core::{future::Future, pin::Pin};

pub type BoxFuture<'t, T = ()> = Pin<Box<dyn Future<Output = T> + Send + 't>>;
pub type LocalBoxFuture<'t, T = ()> = Pin<Box<dyn Future<Output = T> + 't>>;
