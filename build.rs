use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let out = &PathBuf::from(env::var("OUT_DIR").unwrap());
    
    // Read the local memory.x file from project root
    let memory_x = std::fs::read("memory.x")
        .expect("Failed to read memory.x from project root. Ensure it exists next to Cargo.toml");

    // Write it into the build output directory
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(&memory_x)
        .unwrap();

    // Pass the output directory path to the compiler linker flag
    println!("cargo:rustc-link-search={}", out.display());

    // Only re-run this build script if memory.x or build.rs changes
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
}

