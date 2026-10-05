use std::{fs, path::PathBuf, process::Command};

#[test]
fn compute_helpers_are_built_and_sources_are_watched() {
    let scratch = std::env::temp_dir().join(format!("perry-build-artifact-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(scratch.join("out")).unwrap();
    fs::create_dir_all(scratch.join("src/helpers")).unwrap();

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut directories = vec![
        PathBuf::from("src/helpers"),
        PathBuf::from("crates/json-helper"),
        PathBuf::from("crates/time-helper"),
    ];
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
    for source in ["Cargo.toml", "Cargo.lock"] {
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

    let output = Command::new(&script)
        .env("OUT_DIR", scratch.join("out"))
        .env("CARGO_MANIFEST_DIR", &scratch)
        .env("RUSTC", &rustc_bin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "build-script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    for helper in ["search", "text", "fetch", "json", "time"] {
        let bytes = fs::read(scratch.join(format!("out/{helper}.wasm"))).unwrap();
        assert!(wasmparser::Parser::is_core_wasm(&bytes));
    }
    for source in [
        "src/helpers",
        "src/helpers/search.rs",
        "src/helpers/text.rs",
        "src/helpers/fetch.rs",
    ] {
        assert!(stdout.lines().any(
            |line| line == format!("cargo:rerun-if-changed={}", scratch.join(source).display())
        ));
    }
    for source in ["crates/json-helper", "crates/time-helper", "Cargo.lock"] {
        assert!(
            stdout
                .lines()
                .any(|line| line == format!("cargo:rerun-if-changed={source}"))
        );
    }
    fs::remove_dir_all(scratch).unwrap();
}
