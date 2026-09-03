// Sairam.

/// See [`futures::future::BoxFuture`]
pub type BoxFuture<'t, T = ()> = Pin<Box<dyn Future<Output = T> + Send + 't>>;

/// See [`futures::future::LocalBoxFuture`]
pub type LocalBoxFuture<'t, T = ()> = Pin<Box<dyn Future<Output = T> + 't>>;
