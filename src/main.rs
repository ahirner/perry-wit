//! Command-line interface for compiling TypeScript into WASI 0.3 WebAssembly components.

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
    if args.get(1).map(|s| s.as_str()) == Some("gen-types") {
        return run_gen_types(&args[2..]);
    }

    let mut ts_file_path: Option<String> = None;
    let mut out_file_path: Option<String> = None;
    let mut wit_dir_path = "wit".to_string();
    let mut world_name = None;
    let mut core_only = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--out" if i + 1 < args.len() => {
                out_file_path = Some(args[i + 1].clone());
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
                println!("       perry-wit gen-types [OPTIONS]");
                println!();
                println!("Commands:");
                println!(
                    "  gen-types             Generate TypeScript declarations (.d.ts) and tsconfig.json from WIT"
                );
                println!();
                println!("Options:");
                println!("  -o, --out <PATH>      Output WebAssembly file path");
                println!("      --wit <PATH>      WIT definition directory (default: 'wit')");
                println!(
                    "      --world <NAME>    WIT world (required when the package has multiple worlds)"
                );
                println!(
                    "      --core-only       Output Core WebAssembly without component encoding"
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
        wit_dir: PathBuf::from(wit_dir_path),
        world: world_name,
        core_only,
    };

    let ts_file = Path::new(&ts_file_path);
    println!("Compiling {}...", ts_file.display());
    let compiled = compile_file(ts_file, &options)?;
    perry_wit::sdk::write_runtime_declarations(Path::new(".perry/types"))?;

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

fn run_gen_types(args: &[String]) -> Result<()> {
    let mut wit_dir_path = "wit".to_string();
    let mut world_name = None;
    let mut entry = PathBuf::from("src/index.ts");
    let mut out_dir_path = ".perry/types".to_string();
    let mut initialize_tsconfig = true;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-tsconfig" => {
                initialize_tsconfig = false;
                i += 1;
            }
            "-o" | "--out" if i + 1 < args.len() => {
                out_dir_path = args[i + 1].clone();
                i += 2;
            }
            "--entry" if i + 1 < args.len() => {
                entry = PathBuf::from(&args[i + 1]);
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
            "-h" | "--help" => {
                println!("Usage: perry-wit gen-types [OPTIONS]");
                println!();
                println!(
                    "Generates TypeScript declarations (.d.ts) and tsconfig.json from WIT definitions."
                );
                println!();
                println!("Options:");
                println!("      --wit <PATH>      WIT definition directory (default: 'wit')");
                println!(
                    "      --world <NAME>    WIT world (required when the package has multiple worlds)"
                );
                println!(
                    "  -o, --out <DIR>       Output directory for generated types (default: '.perry/types')"
                );
                println!("      --entry <PATH>    Implementation module (default: src/index.ts)");
                println!("      --no-tsconfig    Do not create an authored tsconfig.json");
                println!("  -h, --help            Print help information");
                return Ok(());
            }
            other => {
                bail!(
                    "Unknown argument to gen-types: {other}\nTry 'perry-wit gen-types --help' for usage."
                );
            }
        }
    }

    let options = perry_wit::SdkOptions {
        wit_dir: PathBuf::from(wit_dir_path),
        world: world_name,
        out_dir: PathBuf::from(out_dir_path),
        project_root: None,
        entry,
        initialize_tsconfig,
    };

    let result = perry_wit::generate_sdk_files(&options)?;
    println!(
        "Generated TypeScript declarations in {}",
        result.types_path.display()
    );
    Ok(())
}
