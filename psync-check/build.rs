//! Records the sysroot of the toolchain building the driver: the driver links that toolchain's
//! `librustc_driver`, and `cargo psync` points the dynamic loader at it.
fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = std::process::Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc --print sysroot");
    let sysroot = String::from_utf8(out.stdout).expect("utf-8 sysroot");
    println!("cargo:rustc-env=PSYNC_SYSROOT={}", sysroot.trim());
    println!("cargo:rerun-if-changed=build.rs");
}
