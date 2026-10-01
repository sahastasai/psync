use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-changed=memory.x");
    if env::var("TARGET").as_deref() != Ok("thumbv8m.main-none-eabihf") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rustc-link-arg-examples=-Tlink.x");
}
