use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    // 2. Precompiled artifact in target directory
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let release_artifact =
        manifest_dir.join("target/wasm32-unknown-unknown/release/guest_runtime.wasm");
    if release_artifact.exists() {
        fs::copy(&release_artifact, &target_wasm)
            .expect("Failed to copy target release guest_runtime.wasm");
        println!(
            "cargo:rustc-env=GUEST_RUNTIME_WASM={}",
            target_wasm.display()
        );
        return;
    }

    // 3. Inline compilation fallback during cargo build
    let status = Command::new("cargo")
        .args([
            "rustc",
            "--release",
            "--package",
            "guest-runtime",
            "--target",
            "wasm32-unknown-unknown",
            "--",
            "-C",
            "link-arg=--import-memory",
            "-C",
            "link-arg=--global-base=1048576",
            "-C",
            "link-arg=--no-entry",
        ])
        .status();

    if let Ok(s) = status
        && s.success()
        && release_artifact.exists()
    {
        fs::copy(&release_artifact, &target_wasm)
            .expect("Failed to copy compiled guest_runtime.wasm");
        println!(
            "cargo:rustc-env=GUEST_RUNTIME_WASM={}",
            target_wasm.display()
        );
        return;
    }

    panic!(
        "Failed to locate or build guest_runtime.wasm for embedding.\n\
         Provide GUEST_RUNTIME_PATH or build crates/guest-runtime for wasm32-unknown-unknown."
    );
}
