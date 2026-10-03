use std::{fs, process::Command};

#[test]
fn selected_runtime_artifacts_are_watched_and_copied() {
    let scratch = std::env::temp_dir().join(format!("perry-build-artifact-{}", std::process::id()));
    fs::create_dir_all(scratch.join("out")).unwrap();
    let script = scratch.join("build-script");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target"));
    let build_dir = target_dir.join("debug/build");
    let mut newest_script: Option<(std::path::PathBuf, Option<std::time::SystemTime>)> = None;
    if build_dir.exists() {
        if let Ok(entries) = fs::read_dir(&build_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("perry-wit-") {
                    let candidate = entry.path().join("build-script-build");
                    if candidate.exists() {
                        let mtime = candidate.metadata().and_then(|m| m.modified()).ok();
                        match &newest_script {
                            Some((_, best_mtime)) if mtime > *best_mtime => {
                                newest_script = Some((candidate, mtime));
                            }
                            None => {
                                newest_script = Some((candidate, mtime));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    if let Some((existing, _)) = newest_script {
        fs::copy(existing, &script).unwrap();
    } else {
        panic!("Precompiled Cargo build-script binary not found in target/debug/build");
    }

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
                .env_remove("GUEST_RUNTIME_PATH");
            if relative == "override.wasm" {
                command.env("GUEST_RUNTIME_PATH", &artifact);
            }
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line == format!("cargo:rerun-if-changed={}", artifact.display())
                })
            );
            assert_eq!(
                fs::read(scratch.join("out/guest_runtime.wasm")).unwrap(),
                bytes
            );
        }
    }
    fs::remove_dir_all(scratch).unwrap();
}
