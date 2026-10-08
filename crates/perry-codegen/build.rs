//! Build `perry-runtime` for wasm32 and embed it, like the helper libraries in the root
//! `build.rs`. The runtime owns and exports linear memory, so it is linked as a normal
//! (non-PIC) module with a 64 KiB stack: it needs almost no stack, and the default 1 MiB
//! would be committed memory in every instance.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace = manifest.join("../..");
    println!("cargo:rerun-if-changed=../perry-runtime");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    let target = out.join("runtime-build");
    let status = Command::new(env::var_os("CARGO").expect("Cargo sets CARGO"))
        .current_dir(&workspace)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .args([
            "rustc",
            "--locked",
            "--offline",
            "--release",
            "--package=perry-runtime",
            "--crate-type=cdylib",
            "--target=wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target)
        .args([
            "--",
            "-Clink-arg=-zstack-size=65536",
            "-Clink-arg=--strip-debug",
        ])
        .status()
        .expect("build perry-runtime");
    assert!(status.success(), "compile perry-runtime for wasm32");
    let module = out.join("perry_runtime.wasm");
    fs::copy(
        target.join("wasm32-unknown-unknown/release/perry_runtime.wasm"),
        &module,
    )
    .expect("copy perry-runtime");
    let wasm = fs::read(module).expect("read perry-runtime");
    wasmparser::Validator::new()
        .validate_all(&wasm)
        .expect("validate perry-runtime");
}
