use std::{fs, process::Command};

#[test]
fn selected_runtime_artifacts_are_watched_and_copied() {
    let scratch = std::env::temp_dir().join(format!("perry-build-artifact-{}", std::process::id()));
    fs::create_dir_all(scratch.join("out")).unwrap();
    let script = scratch.join("build-script");
    assert!(Command::new("rustc")
        .args(["build.rs", "-o"])
        .arg(&script)
        .status()
        .unwrap()
        .success());

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
            assert!(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line == format!("cargo:rerun-if-changed={}", artifact.display())
            }));
            assert_eq!(fs::read(scratch.join("out/guest_runtime.wasm")).unwrap(), bytes);
        }
    }
    fs::remove_dir_all(scratch).unwrap();
}
