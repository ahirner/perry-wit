use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/helpers").display()
    );
    for helper in ["search", "text", "fetch", "number"] {
        let source = manifest_dir.join(format!("src/helpers/{helper}.rs"));
        if source.exists() {
            compile_helper(&manifest_dir, &out_dir, helper);
        }
    }
    for helper in ["json", "time"] {
        compile_cargo_helper(&manifest_dir, &out_dir, helper);
    }
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
            "-Clto=fat",
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

fn compile_cargo_helper(manifest: &Path, output: &Path, helper: &str) {
    println!("cargo:rerun-if-changed=crates/{helper}-helper");
    println!("cargo:rerun-if-changed=Cargo.lock");
    let target = output.join("cargo-helper-build");
    let status = Command::new(env::var_os("CARGO").expect("Cargo sets CARGO"))
        .current_dir(manifest)
        .env("CARGO_ENCODED_RUSTFLAGS", "-Crelocation-model=pic")
        .env_remove("RUSTFLAGS")
        .args([
            "rustc",
            "--locked",
            "--offline",
            "--release",
            &format!("--package=perry-{helper}-helper"),
            "--crate-type=cdylib",
            "--target=wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target)
        .args([
            "--",
            "-Clto=fat",
            "-Clink-arg=--shared",
            "-Clink-arg=--no-entry",
            "-Clink-arg=--import-memory",
        ])
        .status()
        .expect("build embedded Cargo helper");
    assert!(status.success(), "compile embedded {helper} helper");
    let source = target.join(format!(
        "wasm32-unknown-unknown/release/perry_{helper}_helper.wasm"
    ));
    let module = output.join(format!("{helper}.wasm"));
    fs::copy(source, &module).expect("copy embedded Cargo helper");
    let wasm = fs::read(module).expect("read embedded Cargo helper");
    wasmparser::Validator::new()
        .validate_all(&wasm)
        .expect("validate embedded Cargo helper");
}
