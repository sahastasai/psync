//! A dependency whose function names do not say what they do.

/// Writes a marker file: an effect its name does not reveal (`stamp` is no verb psync knows).
pub async fn stamp(dir: &str) -> std::io::Result<()> {
    std::fs::write(format!("{dir}/.synced"), b"1")
}

/// Only reads.
pub async fn inspect(path: &str) -> std::io::Result<usize> {
    std::fs::read(path).map(|b| b.len())
}
