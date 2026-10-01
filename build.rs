use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=GUEST_RUNTIME_PATH");
    println!("cargo:rerun-if-changed=crates/guest-runtime/Cargo.toml");
    println!("cargo:rerun-if-changed=crates/guest-runtime/src");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let target_wasm = out_dir.join("guest_runtime.wasm");

    // 1. Explicit build-time override path (e.g. from Nix or external toolchain)
    if let Ok(override_path) = env::var("GUEST_RUNTIME_PATH") {
        let p = Path::new(&override_path);
        if p.exists() {
            fs::copy(p, &target_wasm).expect("Failed to copy GUEST_RUNTIME_PATH artifact");
            println!(
                "cargo:rustc-env=GUEST_RUNTIME_WASM={}",
                target_wasm.display()
            );
            return;
        }
    }

    // 2. Precompiled artifact in target directory (release or debug)
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join("target/wasm32-unknown-unknown/release/guest_runtime.wasm"),
        manifest_dir.join("target/wasm32-unknown-unknown/debug/guest_runtime.wasm"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            fs::copy(&candidate, &target_wasm)
                .expect("Failed to copy precompiled guest_runtime.wasm");
            println!(
                "cargo:rustc-env=GUEST_RUNTIME_WASM={}",
                target_wasm.display()
            );
            return;
        }
    }

    panic!(
        "\n\
         =========================================================================\n\
         Precompiled guest runtime ('guest_runtime.wasm') was not found.\n\
         \n\
         Build it first before compiling `perry-wit`:\n\
           cargo rustc --release --package guest-runtime --target wasm32-unknown-unknown -- \\\n\
             -C link-arg=--import-memory -C link-arg=--global-base=1048576 -C link-arg=--no-entry\n\
         Or pass its path explicitly:\n\
           export GUEST_RUNTIME_PATH=/path/to/guest_runtime.wasm\n\
         Or use `scripts/build.sh` or `nix build`.\n\
         =========================================================================\n"
    );
}
