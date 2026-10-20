//! Command-line interface for compiling TypeScript into WASI 0.3 WebAssembly components.

#[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
mod wasm;

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
    let mut options = CompileOptions {
        wit_dir: PathBuf::from("wit"),
        world: None,
        core_only: false,
    };
    let mut explain_tiers = false;
    let mut keep_names = false;

    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-o" | "--out" if !iter.as_slice().is_empty() => out_file_path = iter.next().cloned(),
            "--wit" if !iter.as_slice().is_empty() => {
                options.wit_dir = PathBuf::from(iter.next().unwrap())
            }
            "--world" if !iter.as_slice().is_empty() => options.world = iter.next().cloned(),
            "--explain-tiers" => explain_tiers = true,
            "--keep-names" => keep_names = true,
            "--core-only" => options.core_only = true,
            arg if !arg.starts_with('-') => ts_file_path = Some(arg.to_string()),
            "-h" | "--help" => {
                print!(
                    "Usage: perry-wit [OPTIONS] <input.ts>\n       perry-wit gen-types [OPTIONS]\n\n                     Commands:\n  gen-types             Generate TypeScript declarations (.d.ts) and tsconfig.json from WIT\n\n                     Options:\n  -o, --out <PATH>      Output WebAssembly file path\n                           --wit <PATH>      WIT definition directory (default: 'wit')\n                           --world <NAME>    WIT world (required when the package has multiple worlds)\n                           --core-only       Output the selected WIT world’s core module\n                           --explain-tiers   Explain allocation lifetimes and stream reuse\n                           --keep-names      Preserve function names for guest profiling\n                       -h, --help            Print help information\n"
                );
                return Ok(());
            }
            other => bail!("Unknown argument: {other}\nTry 'perry-wit --help' for usage."),
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
        options.core_only = true;
    }

    let ts_file = Path::new(&ts_file_path);
    println!("Compiling {}...", ts_file.display());
    let compiled = compile_file(ts_file, &options)?;
    if explain_tiers {
        print!("{}", compiled.allocation_report);
    }
    perry_wit::sdk::write_runtime_declarations(Path::new(".perry/types"))?;

    if let Some(parent) = Path::new(&out_file_path).parent() {
        fs::create_dir_all(parent)?;
    }

    if options.core_only {
        println!(
            "Writing Core Wasm ({} bytes) -> {}",
            compiled.core.len(),
            out_file_path
        );
        fs::write(&out_file_path, &compiled.core)?;
        return Ok(());
    }

    let component = compiled
        .component
        .as_ref()
        .context("Missing component output")?;
    let output = if keep_names {
        &component.bytes
    } else {
        &component.stripped
    };
    fs::write(&out_file_path, output)?;
    println!(
        "Component built: raw = {} bytes, stripped = {} bytes, written = {} bytes -> {}",
        component.bytes.len(),
        component.stripped.len(),
        output.len(),
        out_file_path
    );

    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(output)
        .context("Validating component")?;

    Ok(())
}

fn run_gen_types(args: &[String]) -> Result<()> {
    let mut options = perry_wit::SdkOptions::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-tsconfig" => options.initialize_tsconfig = false,
            "-o" | "--out" if !iter.as_slice().is_empty() => {
                options.out_dir = PathBuf::from(iter.next().unwrap())
            }
            "--entry" if !iter.as_slice().is_empty() => {
                options.entry = PathBuf::from(iter.next().unwrap())
            }
            "--wit" if !iter.as_slice().is_empty() => {
                options.wit_dir = PathBuf::from(iter.next().unwrap())
            }
            "--world" if !iter.as_slice().is_empty() => options.world = iter.next().cloned(),
            "-h" | "--help" => {
                print!(
                    "Usage: perry-wit gen-types [OPTIONS]\n\n                     Generates TypeScript declarations (.d.ts) and tsconfig.json from WIT definitions.\n\n                     Options:\n                           --wit <PATH>      WIT definition directory (default: 'wit')\n                           --world <NAME>    WIT world (required when the package has multiple worlds)\n                       -o, --out <DIR>       Output directory for generated types (default: '.perry/types')\n                           --entry <PATH>    Implementation module (default: src/index.ts)\n                           --no-tsconfig    Do not create an authored tsconfig.json\n                       -h, --help            Print help information\n"
                );
                return Ok(());
            }
            other => bail!(
                "Unknown argument to gen-types: {other}\nTry 'perry-wit gen-types --help' for usage."
            ),
        }
    }

    let result = perry_wit::generate_sdk_files(&options)?;
    println!(
        "Generated TypeScript declarations in {}",
        result.types_path.display()
    );
    Ok(())
}
