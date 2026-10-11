use std::{fs, path::PathBuf, process::Command};

const HELPERS: &[(&str, &[u8])] = &[
    (
        "search",
        include_bytes!(concat!(env!("OUT_DIR"), "/search.wasm")),
    ),
    (
        "text",
        include_bytes!(concat!(env!("OUT_DIR"), "/text.wasm")),
    ),
    (
        "fetch",
        include_bytes!(concat!(env!("OUT_DIR"), "/fetch.wasm")),
    ),
    (
        "json",
        include_bytes!(concat!(env!("OUT_DIR"), "/json.wasm")),
    ),
    (
        "time",
        include_bytes!(concat!(env!("OUT_DIR"), "/time.wasm")),
    ),
    (
        "number",
        include_bytes!(concat!(env!("OUT_DIR"), "/number.wasm")),
    ),
    (
        "runtime",
        include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm")),
    ),
    (
        "runtime-collectible",
        include_bytes!(concat!(env!("OUT_DIR"), "/runtime-collectible.wasm")),
    ),
    ("std", include_bytes!(concat!(env!("OUT_DIR"), "/std.wasm"))),
    ("fs", include_bytes!(concat!(env!("OUT_DIR"), "/fs.wasm"))),
    ("web", include_bytes!(concat!(env!("OUT_DIR"), "/web.wasm"))),
];

#[test]
fn embedded_helpers_are_valid() {
    for (name, bytes) in HELPERS {
        assert!(wasmparser::Parser::is_core_wasm(bytes), "{name}");
        wasmparser::Validator::new().validate_all(bytes).unwrap();
    }
}

#[test]
#[ignore = "cold packaging build; run explicitly in the Nix environment"]
fn compute_helpers_are_built_and_sources_are_watched() {
    let directory = tempfile::tempdir().unwrap();
    let scratch = directory.path();
    fs::create_dir_all(scratch.join("out")).unwrap();
    fs::create_dir_all(scratch.join("src/helpers")).unwrap();

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut directories = vec![PathBuf::from("src/helpers"), PathBuf::from("crates")];
    while let Some(directory) = directories.pop() {
        fs::create_dir_all(scratch.join(&directory)).unwrap();
        for entry in fs::read_dir(repo_root.join(&directory)).unwrap() {
            let entry = entry.unwrap();
            let path = directory.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                directories.push(path);
            } else {
                fs::copy(entry.path(), scratch.join(path)).unwrap();
            }
        }
    }
    for source in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        fs::copy(repo_root.join(source), scratch.join(source)).unwrap();
    }
    for source in ["src/lib.rs", "src/main.rs"] {
        let destination = scratch.join(source);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, "").unwrap();
    }

    let test_binary = std::env::current_exe().expect("locate running test binary");
    let deps_dir = test_binary
        .parent()
        .expect("test binary is in Cargo's deps directory");

    let script = scratch.join("build-script");
    let rustc_bin = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());

    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = fs::read_dir(deps_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(file_name) = path.file_name().and_then(|f| f.to_str())
                && file_name.starts_with("libwasmparser-")
                && file_name.ends_with(".rlib")
            {
                let mtime = path
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                candidates.push((path, mtime));
            }
        }
    }
    candidates.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));

    let mut compiled = false;
    let mut diagnostics = String::new();
    for (rlib, _) in candidates {
        // Release dependencies may contain LLVM bitcode instead of native objects.
        for lto in [false, true] {
            let mut rustc_cmd = Command::new(&rustc_bin);
            rustc_cmd
                .arg("build.rs")
                .arg("--crate-type=bin")
                .arg("-Cpanic=abort")
                .arg("-o")
                .arg(&script)
                .arg("-L")
                .arg(format!("dependency={}", deps_dir.display()))
                .arg("--extern")
                .arg(format!("wasmparser={}", rlib.display()));
            if lto {
                rustc_cmd.arg("-Clto=fat");
            }
            let output = rustc_cmd.output().expect("run rustc for build.rs");
            if output.status.success() {
                compiled = true;
                break;
            }
            diagnostics.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if compiled {
            break;
        }
    }
    assert!(
        compiled,
        "Failed to compile build.rs with rustc using {}: {diagnostics}",
        deps_dir.display()
    );

    let mut command = Command::new(&script);
    command
        .env("OUT_DIR", scratch.join("out"))
        .env("CARGO_MANIFEST_DIR", scratch)
        .env("RUSTC", &rustc_bin)
        .env_remove("PERRY_HELPER_TARGET_DIR");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "build-script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    for (helper, _) in HELPERS {
        let bytes = fs::read(scratch.join(format!("out/{helper}.wasm"))).unwrap();
        assert!(wasmparser::Parser::is_core_wasm(&bytes));
        wasmparser::Validator::new().validate_all(&bytes).unwrap();
    }
    for source in [
        "src/helpers",
        "crates/search-helper",
        "crates/text-helper",
        "crates/fetch-helper",
        "crates/json-helper",
        "crates/time-helper",
        "crates/number-helper",
        "crates/perry-abi",
        "crates/perry-runtime",
        "crates/perry-std",
        "crates/perry-fs",
        "crates/perry-web",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
    ] {
        assert!(
            stdout
                .lines()
                .any(|line| line == format!("cargo:rerun-if-changed={source}"))
        );
    }
    assert!(
        stdout
            .lines()
            .any(|line| line == "cargo:rerun-if-env-changed=PERRY_HELPER_TARGET_DIR")
    );

    let output = command.output().unwrap();
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{diagnostics}");
    assert!(
        !diagnostics.contains("Compiling "),
        "unchanged helpers rebuilt: {diagnostics}"
    );
}
