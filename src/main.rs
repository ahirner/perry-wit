use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use perry_codegen_wasm::compile_modules_to_wasm;
use perry_hir::lower_module;
use perry_parser::parse_typescript;

fn main() -> Result<()> {
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
                eprintln!("Unknown argument: {other}");
                i += 1;
            }
        }
    }

    let Some(ts_file_path) = ts_file_path else {
        eprintln!("Error: missing input TypeScript file");
        eprintln!("Usage: perry-wit [OPTIONS] <input.ts>");
        eprintln!("Try 'perry-wit --help' for more information.");
        std::process::exit(1);
    };

    let out_file_path = out_file_path.unwrap_or_else(|| {
        let p = Path::new(&ts_file_path);
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        format!("dist/{stem}.wasm")
    });

    if out_file_path.ends_with(".core.wasm") {
        core_only = true;
    }

    let ts_file = Path::new(&ts_file_path);
    let ts_content = fs::read_to_string(ts_file)
        .with_context(|| format!("Failed to read TypeScript source {}", ts_file.display()))?;
    println!(
        "Compiling {} ({} bytes)...",
        ts_file.display(),
        ts_content.len()
    );

    let file_name = ts_file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("module.ts");
    let ast = parse_typescript(&ts_content, file_name)
        .map_err(|e| anyhow::anyhow!("Failed to parse {}: {e:?}", ts_file.display()))?;

    let mut hir = lower_module(&ast, "main", file_name)
        .map_err(|e| anyhow::anyhow!("Failed to lower {}: {e:?}", ts_file.display()))?;

    println!(
        "Lowered AST to HIR. Functions: {}, Inits: {}",
        hir.functions.len(),
        hir.init.len()
    );

    // Rewrite object spread IIFEs into native Expr::ObjectAssign
    fn rewrite_expr(expr: &mut perry_hir::ir::Expr) {
        if let perry_hir::ir::Expr::JsonStringifyFull(val, _, _) = expr {
            *expr = perry_hir::ir::Expr::JsonStringify(val.clone());
            return;
        }
        if let perry_hir::ir::Expr::Call { callee, args, .. } = expr {
            if let perry_hir::ir::Expr::PropertyGet {
                object, property, ..
            } = callee.as_ref()
            {
                if property == "json" {
                    *expr = perry_hir::ir::Expr::NativeMethodCall {
                        module: "fetch".to_string(),
                        class_name: Some("Response".to_string()),
                        object: Some(object.clone()),
                        method: "json".to_string(),
                        args: args.clone(),
                    };
                    return;
                }
            }
            if let perry_hir::ir::Expr::Closure { body, .. } = callee.as_mut() {
                let mut sources = Vec::new();
                let mut is_spread_iife = !body.is_empty();
                for stmt in body.iter() {
                    match stmt {
                        perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::Call {
                            callee: inner_callee,
                            args: inner_args,
                            ..
                        }) => {
                            if let perry_hir::ir::Expr::ExternFuncRef { name, .. } =
                                inner_callee.as_ref()
                            {
                                if name == "js_object_assign_one" && inner_args.len() >= 2 {
                                    sources.push(inner_args[1].clone());
                                    continue;
                                }
                            }
                            is_spread_iife = false;
                            break;
                        }
                        perry_hir::ir::Stmt::Return(_) => {}
                        _ => {
                            is_spread_iife = false;
                            break;
                        }
                    }
                }
                if is_spread_iife && !sources.is_empty() && !args.is_empty() {
                    let target = args.remove(0);
                    *expr = perry_hir::ir::Expr::ObjectAssign {
                        target: Box::new(target),
                        sources,
                    };
                    return;
                }
            }
        }
        perry_hir::walker::walk_expr_children_mut(expr, &mut rewrite_expr);
    }

    fn rewrite_stmt(stmt: &mut perry_hir::ir::Stmt) {
        match stmt {
            perry_hir::ir::Stmt::Expr(e)
            | perry_hir::ir::Stmt::Return(Some(e))
            | perry_hir::ir::Stmt::Throw(e) => {
                rewrite_expr(e);
            }
            perry_hir::ir::Stmt::Let { init, .. } => {
                if let Some(e) = init {
                    rewrite_expr(e);
                }
            }
            _ => {}
        }
    }

    for stmt in &mut hir.init {
        rewrite_stmt(stmt);
    }
    for func in &mut hir.functions {
        for stmt in &mut func.body {
            rewrite_stmt(stmt);
        }
    }

    let wasm_bytes = compile_modules_to_wasm(&[("main".to_string(), hir)])
        .map_err(|e| anyhow::anyhow!("Compilation failed: {e:?}"))?;

    let wasm_bytes = add_wasi_cli_run_export(&wasm_bytes)?;

    if let Some(parent) = Path::new(&out_file_path).parent() {
        fs::create_dir_all(parent)?;
    }

    if core_only {
        println!(
            "Writing Core Wasm ({} bytes) -> {}",
            wasm_bytes.len(),
            out_file_path
        );
        fs::write(&out_file_path, &wasm_bytes)?;
        return Ok(());
    }

    // Full in-process pipeline: Link -> Embed WIT -> Component Encode -> Strip
    let rt_path = match runtime_wasm_path {
        Some(p) => PathBuf::from(p),
        None => ensure_guest_runtime_compiled()?,
    };

    println!("Linking with guest runtime: {}...", rt_path.display());
    let rt_bytes = fs::read(&rt_path)
        .with_context(|| format!("Reading guest runtime from {}", rt_path.display()))?;

    let merged_core = perry_wit::linker::merge_core_modules(&wasm_bytes, &rt_bytes)
        .context("Linking TypeScript core wasm with guest runtime")?;
    println!("Linked into unified Core Wasm: {} bytes", merged_core.len());

    println!(
        "Embedding WIT ({}) and encoding component (world: {:?})...",
        wit_dir_path, world_name
    );
    let component_bytes = perry_wit::component::embed_and_encode(
        &merged_core,
        Path::new(&wit_dir_path),
        world_name.as_deref(),
    )?;

    let stripped_bytes = perry_wit::strip::component(&component_bytes)
        .context("Stripping custom sections from component")?;

    println!(
        "Component built: raw = {} bytes, stripped = {} bytes -> {}",
        component_bytes.len(),
        stripped_bytes.len(),
        out_file_path
    );
    fs::write(&out_file_path, &stripped_bytes)?;

    println!("Validating component with wasmparser...");
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    validator
        .validate_all(&stripped_bytes)
        .context("Validating stripped component")?;
    println!("Component validated successfully!");

    Ok(())
}

fn ensure_guest_runtime_compiled() -> Result<PathBuf> {
    let default_path = PathBuf::from("target/wasm32-unknown-unknown/release/guest_runtime.wasm");
    if default_path.exists() {
        return Ok(default_path);
    }

    println!("guest_runtime.wasm not found; compiling guest-runtime crate...");
    let status = Command::new("cargo")
        .args([
            "rustc",
            "--release",
            "--package",
            "guest-runtime",
            "--target",
            "wasm32-unknown-unknown",
            "--",
            "-C",
            "link-arg=--import-memory",
            "-C",
            "link-arg=--global-base=1048576",
            "-C",
            "link-arg=--no-entry",
        ])
        .status()
        .context("Running cargo rustc for guest-runtime")?;

    ensure!(status.success(), "Failed to compile guest-runtime crate");
    ensure!(
        default_path.exists(),
        "guest_runtime.wasm still not found at {}",
        default_path.display()
    );

    Ok(default_path)
}

fn add_wasi_cli_run_export(wasm_bytes: &[u8]) -> Result<Vec<u8>> {
    let wat = wasmprinter::print_bytes(wasm_bytes)
        .map_err(|e| anyhow::anyhow!("wasmprinter failed: {e}"))?;

    // Ensure memory has enough pages for guest runtime (at least 32 pages = 2MB)
    let mut wat = wat.replace("(memory (;0;) 2)", "(memory (;0;) 32)");
    let pattern = "(export \"_start\" (func ";
    let idx = wat
        .find(pattern)
        .ok_or_else(|| anyhow::anyhow!("Could not find _start export in wat"))?;
    let rest = &wat[idx + pattern.len()..];
    let close = rest
        .find(')')
        .ok_or_else(|| anyhow::anyhow!("Malformed _start export in wat"))?;
    let start_func_ref = rest[..close].trim();
    let clean_func_ref = start_func_ref.trim_matches(|c| c == '(' || c == ';' || c == ')');

    let last_paren = wat
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("No closing paren in wat"))?;

    let wrapper = format!(
        "\n  (func $wasi_cli_run (result i32)\n    call {}\n    i32.const 0\n  )\n  (export \"wasi:cli/run@0.2.6#run\" (func $wasi_cli_run))\n",
        clean_func_ref
    );

    wat.insert_str(last_paren, &wrapper);

    wat::parse_str(&wat)
        .map_err(|e| anyhow::anyhow!("Failed to re-parse wat with wasi:cli/run wrapper: {e}"))
}
