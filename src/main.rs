//! Command-line interface for compiling TypeScript into WASI Preview 2 WebAssembly components.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use perry_wit::compiler::{CompileOptions, compile_file};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("perry-wit: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut ts_file_path: Option<String> = None;
    let mut out_file_path: Option<String> = None;
    let mut runtime_wasm_path: Option<String> = None;
    let mut wit_dir_path = "wit".to_string();
    let mut world_name = Some("merge-docs".to_string());
    let mut core_only = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--out" if i + 1 < args.len() => {
                out_file_path = Some(args[i + 1].clone());
                i += 2;
            }
            "--runtime" if i + 1 < args.len() => {
                runtime_wasm_path = Some(args[i + 1].clone());
                i += 2;
            }
            "--wit" if i + 1 < args.len() => {
                wit_dir_path = args[i + 1].clone();
                i += 2;
            }
            "--world" if i + 1 < args.len() => {
                world_name = Some(args[i + 1].clone());
                i += 2;
            }
            "--core-only" => {
                core_only = true;
                i += 1;
            }
            arg if !arg.starts_with('-') => {
                ts_file_path = Some(arg.to_string());
                i += 1;
            }
            "-h" | "--help" => {
                println!("Usage: perry-wit [OPTIONS] <input.ts>");
                println!();
                println!("Options:");
                println!("  -o, --out <PATH>      Output WebAssembly file path");
                println!("      --runtime <PATH>  Guest runtime WASM module path");
                println!("      --wit <PATH>      WIT definition directory (default: 'wit')");
                println!(
                    "      --world <NAME>    WIT world name to target (default: 'merge-docs')"
                );
                println!(
                    "      --core-only       Output linked Core WebAssembly without component encoding"
                );
                println!("  -h, --help            Print help information");
                return Ok(());
            }
            other => {
                bail!("Unknown argument: {other}\nTry 'perry-wit --help' for usage.");
            }
        }
    }

    let Some(ts_file_path) = ts_file_path else {
        bail!(
            "missing input TypeScript file\nUsage: perry-wit [OPTIONS] <input.ts>\nTry 'perry-wit --help' for more information."
        );
    };

    let out_file_path = out_file_path.unwrap_or_else(|| {
        let p = Path::new(&ts_file_path);
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        format!("dist/{stem}.wasm")
    });

    if out_file_path.ends_with(".core.wasm") {
        core_only = true;
    }

    let options = CompileOptions {
        out_path: Some(PathBuf::from(&out_file_path)),
        runtime_path: runtime_wasm_path.map(PathBuf::from),
        wit_dir: PathBuf::from(wit_dir_path),
        world: world_name,
        core_only,
    };

    let ts_file = Path::new(&ts_file_path);
    println!("Compiling {}...", ts_file.display());
    let compiled = compile_file(ts_file, &options)?;

    if let Some(parent) = Path::new(&out_file_path).parent() {
        fs::create_dir_all(parent)?;
    }

    if core_only {
        println!(
            "Writing Core Wasm ({} bytes) -> {}",
            compiled.core.len(),
            out_file_path
        );
        fs::write(&out_file_path, &compiled.core)?;
        return Ok(());
    }

    let stripped_bytes = compiled
        .stripped
        .as_ref()
        .context("Missing stripped component bytes")?;

    fs::write(&out_file_path, stripped_bytes)?;
    println!(
        "Component built: raw = {} bytes, stripped = {} bytes -> {}",
        compiled.component.as_ref().map(|c| c.len()).unwrap_or(0),
        stripped_bytes.len(),
        out_file_path
    );

    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(stripped_bytes)
        .context("Validating stripped component")?;

    Ok(())
}
