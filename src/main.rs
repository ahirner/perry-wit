use anyhow::Result;
use perry_codegen_wasm::compile_modules_to_wasm;
use perry_hir::lower_module;
use perry_parser::parse_typescript;
use std::fs;
use std::path::Path;
use wasmparser::Parser;
use wasmparser::Payload;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let ts_file_path = if args.len() > 1 && !args[1].starts_with('-') {
        &args[1]
    } else {
        "examples/merge_docs.ts"
    };

    let mut out_file_path = "dist/merge_docs.core.wasm".to_string();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "-o" && i + 1 < args.len() {
            out_file_path = args[i + 1].clone();
            i += 2;
        } else {
            i += 1;
        }
    }

    let ts_file = Path::new(ts_file_path);
    let ts_content = fs::read_to_string(ts_file)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", ts_file.display()))?;
    println!("Compiling {} ({} bytes)...", ts_file.display(), ts_content.len());

    let file_name = ts_file.file_name().and_then(|s| s.to_str()).unwrap_or("module.ts");
    let ast = parse_typescript(&ts_content, file_name)
        .map_err(|e| anyhow::anyhow!("Failed to parse: {e:?}"))?;

    let mut hir = lower_module(&ast, "main", file_name)
        .map_err(|e| anyhow::anyhow!("Failed to lower: {e:?}"))?;

    println!("Lowered AST to HIR. Functions: {}, Inits: {}", hir.functions.len(), hir.init.len());

    // Rewrite object spread IIFEs into native Expr::ObjectAssign
    fn rewrite_expr(expr: &mut perry_hir::ir::Expr) {
        if let perry_hir::ir::Expr::JsonStringifyFull(val, _, _) = expr {
            *expr = perry_hir::ir::Expr::JsonStringify(val.clone());
            return;
        }
        if let perry_hir::ir::Expr::Call { callee, args, .. } = expr {
            if let perry_hir::ir::Expr::PropertyGet { object, property, .. } = callee.as_ref() {
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
                            if let perry_hir::ir::Expr::ExternFuncRef { name, .. } = inner_callee.as_ref() {
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
            perry_hir::ir::Stmt::Expr(e) | perry_hir::ir::Stmt::Return(Some(e)) | perry_hir::ir::Stmt::Throw(e) => {
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

    println!("Compiled to Core Wasm: {} bytes -> {}", wasm_bytes.len(), out_file_path);
    fs::write(&out_file_path, &wasm_bytes)?;

    println!("Imports in generated Core Wasm:");
    let mut import_count = 0;
    for payload in Parser::new(0).parse_all(&wasm_bytes) {
        if let Payload::ImportSection(s) = payload? {
            for import_group in s {
                if let wasmparser::Imports::Single(_, imp) = import_group? {
                    import_count += 1;
                    if import_count <= 10 {
                        println!("  {}:{}", imp.module, imp.name);
                    }
                }
            }
        }
    }
    if import_count > 10 {
        println!("  ... and {} more imports (total: {})", import_count - 10, import_count);
    }

    Ok(())
}
