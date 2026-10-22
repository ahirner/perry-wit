//! Command-line interface for compiling TypeScript into WASI 0.3 WebAssembly components.

#[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
mod wasm;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use perry_wit::compiler::{CompileOptions, compile_file};

const HELP: &str = "\
Usage: perry-wit [OPTIONS] <input.ts>
       perry-wit gen-types [OPTIONS]

Commands:
  gen-types             Generate TypeScript declarations (.d.ts) and tsconfig.json from WIT

Options:
  -o, --out <PATH>      Output WebAssembly file path (default: 'dist/<stem>.wasm')
      --wit <PATH>      WIT definition directory (default: 'wit')
      --world <NAME>    WIT world (required when the package has multiple worlds)
      --core-only       Output the selected WIT world's core module
      --explain-tiers   Explain allocation lifetimes and stream reuse
      --keep-names      Preserve function names for guest profiling
  -h, --help            Print help information
";

const GEN_TYPES_HELP: &str = "\
Usage: perry-wit gen-types [OPTIONS]

Generates TypeScript declarations (.d.ts) and tsconfig.json from WIT definitions.

Options:
      --wit <PATH>      WIT definition directory (default: 'wit')
      --world <NAME>    WIT world (required when the package has multiple worlds)
  -o, --out <DIR>       Output directory for generated types (default: '.perry/types')
      --entry <PATH>    Implementation module (default: 'src/index.ts')
      --no-tsconfig     Do not create an authored tsconfig.json
  -h, --help            Print help information
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(compile_error) = error.downcast_ref::<perry_wit::CompileError>() {
                eprintln!("perry-wit: {compile_error}");
            } else {
                eprintln!("perry-wit: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn take_value(
    arg: &str,
    inline: Option<&str>,
    iter: &mut std::slice::Iter<'_, String>,
    placeholder: &str,
    help_cmd: &str,
) -> Result<String> {
    let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
    match inline.or_else(|| iter.next().map(String::as_str)) {
        Some(v) if !v.is_empty() && (inline.is_some() || !v.starts_with('-')) => Ok(v.to_owned()),
        _ => bail!(
            "Option '{flag}' requires a value ({flag} {placeholder})\nTry '{help_cmd}' for usage."
        ),
    }
}

fn split_flag(arg: &str) -> (&str, Option<&str>) {
    arg.starts_with("--")
        .then(|| arg.split_once('='))
        .flatten()
        .map_or((arg, None), |(f, v)| (f, Some(v)))
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
        let (flag, inline) = split_flag(arg);
        let mut val = |ph| take_value(arg, inline, &mut iter, ph, "perry-wit --help");
        match (flag, inline) {
            ("-o" | "--out", _) => out_file_path = Some(val("<PATH>")?),
            ("--wit", _) => options.wit_dir = PathBuf::from(val("<PATH>")?),
            ("--world", _) => options.world = Some(val("<NAME>")?),
            ("--explain-tiers", None) => explain_tiers = true,
            ("--keep-names", None) => keep_names = true,
            ("--core-only", None) => options.core_only = true,
            ("-h" | "--help", None) => {
                print!("{HELP}");
                return Ok(());
            }
            _ if !arg.starts_with('-') => {
                if let Some(existing) = &ts_file_path {
                    bail!(
                        "Unexpected argument '{arg}' (input file already set to '{existing}')\nTry 'perry-wit --help' for usage."
                    );
                }
                ts_file_path = Some(arg.clone());
            }
            _ => bail!("Unknown argument: {arg}\nTry 'perry-wit --help' for usage."),
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
        let (flag, inline) = split_flag(arg);
        let mut val = |ph| take_value(arg, inline, &mut iter, ph, "perry-wit gen-types --help");
        match (flag, inline) {
            ("--no-tsconfig", None) => options.initialize_tsconfig = false,
            ("-o" | "--out", _) => options.out_dir = PathBuf::from(val("<DIR>")?),
            ("--entry", _) => options.entry = PathBuf::from(val("<PATH>")?),
            ("--wit", _) => options.wit_dir = PathBuf::from(val("<PATH>")?),
            ("--world", _) => options.world = Some(val("<NAME>")?),
            ("-h" | "--help", None) => {
                print!("{GEN_TYPES_HELP}");
                return Ok(());
            }
            _ => bail!(
                "Unknown argument to gen-types: {arg}\nTry 'perry-wit gen-types --help' for usage."
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
