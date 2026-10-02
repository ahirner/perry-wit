use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

use perry_wit::compiler::{CompileOptions, compile_typescript};

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "perry-regression-{}-{}",
            std::process::id(),
            NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn compile(&self, source: &str, wit: Option<&str>) -> PathBuf {
        let compiled = self.compile_artifacts(source, wit);
        let path = self.0.join("test.wasm");
        fs::write(&path, compiled.component.unwrap()).unwrap();
        path
    }

    pub fn compile_artifacts(
        &self,
        source: &str,
        wit: Option<&str>,
    ) -> perry_wit::compiler::Compiled {
        let mut options = CompileOptions::default();
        if let Some(wit) = wit {
            options.wit_dir = self.0.join("wit");
            options.world = Some("test".into());
            fs::create_dir_all(&options.wit_dir).unwrap();
            fs::write(options.wit_dir.join("test.wit"), wit).unwrap();
        }
        compile_typescript(source, "test.ts", &options).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn get_wasmtime_path() -> PathBuf {
    if let Ok(path) = std::env::var("WASMTIME") {
        return PathBuf::from(path);
    }
    if Command::new("wasmtime").arg("--version").output().is_ok() {
        return PathBuf::from("wasmtime");
    }
    if let Ok(entries) = std::fs::read_dir("/nix/store") {
        for entry in entries.flatten() {
            let path = entry.path().join("bin/wasmtime");
            if path.exists() {
                return path;
            }
        }
    }
    PathBuf::from("wasmtime")
}

pub fn run(source: &str, wit: Option<&str>, invocation: Option<&str>) -> Output {
    let scratch = Scratch::new();
    let wasm = scratch.compile(source, wit);
    let mut command = Command::new(get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "-S",
        "http=y",
        "-S",
        "inherit-network=y",
    ]);
    if let Some(invocation) = invocation {
        command.args(["--invoke", invocation]);
    }
    command.arg(wasm).output().expect(
        "wasmtime must be available; run tests through nix shell nixpkgs#wasmtime -c cargo test",
    )
}

pub fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "status: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}
