use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    for helper in ["search", "text"] {
        let source = manifest_dir.join(format!("src/helpers/{helper}.rs"));
        if source.exists() {
            compile_helper(&manifest_dir, &out_dir, helper);
        }
    }

    println!("cargo:rerun-if-env-changed=GUEST_RUNTIME_PATH");
    println!("cargo:rerun-if-changed=crates/guest-runtime/Cargo.toml");
    println!("cargo:rerun-if-changed=crates/guest-runtime/src");

    let target_wasm = out_dir.join("guest_runtime.wasm");

    // 1. Explicit build-time override path (e.g. from Nix or external toolchain)
    if let Ok(override_path) = env::var("GUEST_RUNTIME_PATH") {
        let p = Path::new(&override_path);
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
            let _ = fs::remove_file(&target_wasm);
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
            println!("cargo:rerun-if-changed={}", candidate.display());
            let _ = fs::remove_file(&target_wasm);
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

fn compile_helper(manifest: &Path, output: &Path, helper: &str) {
    let source = manifest.join(format!("src/helpers/{helper}.rs"));
    let module = output.join(format!("{helper}.wasm"));
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=RUSTC");
    let status = Command::new(env::var_os("RUSTC").expect("Cargo sets RUSTC"))
        .args([
            "--crate-name",
            helper,
            "--crate-type=cdylib",
            "--edition=2024",
            "--target=wasm32-unknown-unknown",
            "-Copt-level=z",
            "-Cpanic=abort",
            "-Cdebuginfo=0",
            "-Crelocation-model=pic",
            "-Clink-arg=--shared",
            "-Clink-arg=--no-entry",
            "-Clink-arg=--import-memory",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&module)
        .status()
        .expect("run rustc for embedded computation helper");
    assert!(status.success(), "compile embedded {} helper", helper);
    let wasm = fs::read(&module).expect("read embedded helper module");
    wasmparser::Validator::new()
        .validate_all(&wasm)
        .expect("validate embedded helper module");
}
