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
    for helper in ["search", "text", "fetch"] {
        compile_helper(&manifest_dir, &out_dir, helper);
    }
    for helper in ["json", "time", "number"] {
        compile_cargo_helper(
            &manifest_dir,
            &out_dir,
            &format!("perry-{helper}-helper"),
            &format!("crates/{helper}-helper"),
            helper,
        );
    }
    for (package, helper) in [
        ("perry-runtime", "runtime"),
        ("perry-std", "std"),
        ("perry-fs", "fs"),
        ("perry-web", "web"),
    ] {
        compile_cargo_helper(
            &manifest_dir,
            &out_dir,
            package,
            &format!("crates/{package}"),
            helper,
        );
    }
    compile_cargo_helper(
        &manifest_dir,
        &out_dir,
        "perry-runtime",
        "crates/perry-runtime",
        "runtime-collectible",
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
            "-Copt-level=2",
            "-Cpanic=abort",
            "-Cdebuginfo=0",
            "-Crelocation-model=pic",
            "-Clto=fat",
            "-Clink-arg=--shared",
            "-Clink-arg=--no-entry",
            "-Clink-arg=--import-memory",
            "-Clink-arg=--strip-debug",
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

fn compile_cargo_helper(
    manifest: &Path,
    output: &Path,
    package: &str,
    crate_dir: &str,
    helper: &str,
) {
    println!("cargo:rerun-if-changed={crate_dir}");
    println!("cargo:rerun-if-changed=Cargo.lock");
    let target = output.join("cargo-helper-build");
    // Symbols `src/link` resolves: `__heap_base` (end of linked data and stack).
    let undefined = output.join("helper-undefined.txt");
    fs::write(&undefined, "__heap_base\n").expect("write undefined symbol list");
    let mut command = Command::new(env::var_os("CARGO").expect("Cargo sets CARGO"));
    if helper == "runtime-collectible" {
        command.args(["--config", "build.jobs=1"]);
    }
    let command = command
        .current_dir(manifest)
        .env("CARGO_ENCODED_RUSTFLAGS", "-Crelocation-model=pic")
        .env_remove("RUSTFLAGS")
        // Embedded helpers are release builds; incremental compilation would change them.
        .env_remove("CARGO_INCREMENTAL")
        .args([
            "rustc",
            "--locked",
            "--offline",
            "--release",
            &format!("--package={package}"),
            "--crate-type=cdylib",
            "--target=wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target);
    if helper == "runtime-collectible" {
        command.args(["--features", "collectible"]);
    }
    // Keep bounded, allocation-light components small; optimize computation and the
    // collectible runtime for the loops and application work that need them.
    let optimization = if helper == "runtime" {
        "-Copt-level=z"
    } else {
        "-Copt-level=2"
    };
    let status = command
        .args([
            "--",
            optimization,
            "-Clto=fat",
            "-Clink-arg=--shared",
            "-Clink-arg=--no-entry",
            &format!("-Clink-arg=--allow-undefined-file={}", undefined.display()),
            "-Clink-arg=--import-memory",
            "-Clink-arg=--strip-debug",
        ])
        .status()
        .expect("build embedded Cargo helper");
    assert!(status.success(), "compile embedded {helper} helper");
    let source = target.join(format!(
        "wasm32-unknown-unknown/release/{}.wasm",
        package.replace('-', "_")
    ));
    let module = output.join(format!("{helper}.wasm"));
    fs::copy(source, &module).expect("copy embedded Cargo helper");
    let wasm = fs::read(module).expect("read embedded Cargo helper");
    wasmparser::Validator::new()
        .validate_all(&wasm)
        .expect("validate embedded Cargo helper");
}
