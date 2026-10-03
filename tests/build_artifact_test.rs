use std::{fs, path::PathBuf, process::Command};

#[test]
fn selected_runtime_artifacts_are_watched_and_copied() {
    let scratch = std::env::temp_dir().join(format!("perry-build-artifact-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(scratch.join("out")).unwrap();
    fs::create_dir_all(scratch.join("src/helpers")).unwrap();

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    fs::copy(
        repo_root.join("src/helpers/search.rs"),
        scratch.join("src/helpers/search.rs"),
    )
    .unwrap();
    fs::copy(
        repo_root.join("src/helpers/text.rs"),
        scratch.join("src/helpers/text.rs"),
    )
    .unwrap();

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
            if let Some(file_name) = path.file_name().and_then(|f| f.to_str()) {
                if file_name.starts_with("libwasmparser-") && file_name.ends_with(".rlib") {
                    let mtime = path
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    candidates.push((path, mtime));
                }
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

    for relative in [
        "override.wasm",
        "target/wasm32-unknown-unknown/debug/guest_runtime.wasm",
        "target/wasm32-unknown-unknown/release/guest_runtime.wasm",
    ] {
        let artifact = scratch.join(relative);
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        for bytes in [b"first".as_slice(), b"replacement".as_slice()] {
            fs::write(&artifact, bytes).unwrap();
            let mut command = Command::new(&script);
            command
                .env("OUT_DIR", scratch.join("out"))
                .env("CARGO_MANIFEST_DIR", &scratch)
                .env("RUSTC", &rustc_bin)
                .env_remove("GUEST_RUNTIME_PATH");
            if relative == "override.wasm" {
                command.env("GUEST_RUNTIME_PATH", &artifact);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "build-script failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout_str.lines().any(|line| {
                    line == format!("cargo:rerun-if-changed={}", artifact.display())
                }),
                "missing rerun-if-changed for {}",
                artifact.display()
            );
            assert!(
                stdout_str.lines().any(|line| {
                    line == format!(
                        "cargo:rerun-if-changed={}",
                        scratch.join("src/helpers/search.rs").display()
                    )
                }),
                "missing rerun-if-changed for search.rs"
            );
            assert!(
                stdout_str.lines().any(|line| {
                    line == format!(
                        "cargo:rerun-if-changed={}",
                        scratch.join("src/helpers/text.rs").display()
                    )
                }),
                "missing rerun-if-changed for text.rs"
            );
            assert_eq!(
                fs::read(scratch.join("out/guest_runtime.wasm")).unwrap(),
                bytes
            );
            assert!(scratch.join("out/search.wasm").exists());
            assert!(scratch.join("out/text.wasm").exists());
        }
    }
    fs::remove_dir_all(scratch).unwrap();
}
