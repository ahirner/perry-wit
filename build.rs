use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    embed_wasi(&manifest_dir, &out_dir);

    for path in [
        "src/helpers",
        "crates/perry-abi",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=PERRY_HELPER_TARGET_DIR");
    println!("cargo:rerun-if-env-changed=RUSTC");

    for helper in ["text", "fetch", "json", "time", "number"] {
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

fn embed_wasi(manifest: &Path, output: &Path) {
    println!("cargo:rerun-if-env-changed=WASI_WIT_PATH");
    let directory = env::var_os("WASI_WIT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("wit/deps"));
    let mut generated = String::from("pub(super) const PACKAGES: &[&[(&str, &str)]] = &[\n");
    for package in ["cli", "clocks", "filesystem", "http", "random", "sockets"] {
        let path = directory.join(package);
        println!("cargo:rerun-if-changed={}", path.display());
        let mut files = fs::read_dir(&path)
            .unwrap_or_else(|error| {
                panic!(
                    "read pinned WASI package {}: {error}; build inside nix develop",
                    path.display()
                )
            })
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wit"))
            .collect::<Vec<_>>();
        files.sort();
        generated.push_str("&[\n");
        for file in files {
            generated.push_str(&format!(
                "({:?}, {:?}),\n",
                file.file_name().unwrap().to_str().unwrap(),
                fs::read_to_string(&file).unwrap()
            ));
        }
        generated.push_str("],\n");
    }
    generated.push_str("];\n");
    fs::write(output.join("wasi_packages.rs"), generated).unwrap();
}

fn compile_cargo_helper(
    manifest: &Path,
    output: &Path,
    package: &str,
    crate_dir: &str,
    helper: &str,
) {
    println!("cargo:rerun-if-changed={crate_dir}");
    let mut target = env::var_os("PERRY_HELPER_TARGET_DIR")
        .map(|path| manifest.join(path))
        .unwrap_or_else(|| output.join("cargo-helper-build"));
    let collectible = helper == "runtime-collectible";
    if collectible {
        target.push("collectible");
    }
    let undefined = manifest.join("src/helpers/undefined.txt");
    let mut command = Command::new(env::var_os("CARGO").expect("Cargo sets CARGO"));
    command
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
            "--lib",
            &format!("--package={package}"),
            "--crate-type=cdylib",
            "--target=wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target);
    if collectible {
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
