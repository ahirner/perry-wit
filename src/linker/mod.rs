//! In-process WebAssembly core module linker.
//!
//! Merges the TypeScript-generated core module (`ts_core.wasm`) with the
//! compiled guest runtime (`guest_runtime.wasm`), resolving `rt:*` imports
//! directly to guest runtime exports, unifying memory, remapping function/type/global/table
//! indices, and combining data segments.

mod remap;
mod sections;
mod types;

use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    DataSegment, DataSegmentMode, ElementMode, ElementSegment, Elements, ExportKind, Module,
};
use wasmparser::{DataKind, ElementItems, ElementKind, ExternalKind};

use remap::{ReencodeA, ReencodeB, reencode_const_expr};
use sections::{parse_module_a, parse_module_b};
use types::{FuncSig, to_func_sig};

/// Merges `ts_core.wasm` (Module A) and `guest_runtime.wasm` (Module B) into a single Core Wasm module.
pub fn merge_core_modules(ts_wasm: &[u8], runtime_wasm: &[u8]) -> Result<Vec<u8>> {
    let a = parse_module_a(ts_wasm).context("parsing TypeScript core wasm")?;
    let b = parse_module_b(runtime_wasm).context("parsing guest-runtime wasm")?;

    ensure!(!a.memories.is_empty(), "Module A must define memory");

    // 1. Unify types
    let mut merged_sigs = Vec::<FuncSig>::new();
    let mut sig_to_idx = HashMap::<FuncSig, u32>::new();

    let mut type_map_a = Vec::with_capacity(a.types.len());
    for ty in &a.types {
        let sig = to_func_sig(ty)?;
        let idx = *sig_to_idx.entry(sig.clone()).or_insert_with(|| {
            let i = merged_sigs.len() as u32;
            merged_sigs.push(sig);
            i
        });
        type_map_a.push(idx);
    }

    let mut type_map_b = Vec::with_capacity(b.types.len());
    for ty in &b.types {
        let sig = to_func_sig(ty)?;
        let idx = *sig_to_idx.entry(sig.clone()).or_insert_with(|| {
            let i = merged_sigs.len() as u32;
            merged_sigs.push(sig);
            i
        });
        type_map_b.push(idx);
    }

    // 2. Compute Function Index Spaces
    let num_wasi_imports = b.wasi_imports.len() as u32;
    let num_a_imports = a.imports.len() as u32;
    let num_a_funcs = a.func_types.len() as u32;
    let num_b_funcs = b.func_types.len() as u32;

    // Module B function mapping
    let mut func_map_b = Vec::with_capacity((num_wasi_imports + num_b_funcs) as usize);
    // Module B's WASI imports stay at indices 0..num_wasi_imports - 1
    for i in 0..num_wasi_imports {
        func_map_b.push(i);
    }
    // Module B's defined functions come after Module A's defined functions
    for j in 0..num_b_funcs {
        func_map_b.push(num_wasi_imports + num_a_funcs + j);
    }

    // Module A function mapping
    let mut func_map_a = Vec::with_capacity((num_a_imports + num_a_funcs) as usize);
    // Resolve Module A's imports against Module B's exports
    for &(mod_name, name, _ty) in &a.imports {
        ensure!(
            mod_name == "rt",
            "Module A contains unexpected import module '{mod_name}' (expected 'rt')"
        );
        let b_func_idx = b.export_funcs.get(name).copied().with_context(|| {
            format!("Runtime import 'rt:{name}' not found in guest-runtime exports")
        })?;
        let merged_idx = func_map_b
            .get(b_func_idx as usize)
            .copied()
            .with_context(|| {
                format!("Exported function {b_func_idx} for '{name}' out of range in Module B")
            })?;
        func_map_a.push(merged_idx);
    }
    // Module A's defined functions come immediately after WASI imports
    for i in 0..num_a_funcs {
        func_map_a.push(num_wasi_imports + i);
    }

    // 3. Globals Mapping
    let num_a_globals = a.globals.len() as u32;

    // 4. Construct Merged Module
    let mut module = Module::new();

    // Type Section
    let mut type_sec = wasm_encoder::TypeSection::new();
    for sig in &merged_sigs {
        type_sec
            .ty()
            .function(sig.params.clone(), sig.results.clone());
    }
    module.section(&type_sec);

    // Import Section (only Module B's WASI imports remain)
    let mut import_sec = wasm_encoder::ImportSection::new();
    for &(m, n, ty) in &b.wasi_imports {
        let merged_ty = type_map_b[ty as usize];
        import_sec.import(m, n, wasm_encoder::EntityType::Function(merged_ty));
    }
    module.section(&import_sec);

    // Function Section (types for defined functions)
    let mut func_sec = wasm_encoder::FunctionSection::new();
    // First, Module A's defined functions
    for &ty in &a.func_types {
        func_sec.function(type_map_a[ty as usize]);
    }
    // Second, Module B's defined functions
    for &ty in &b.func_types {
        func_sec.function(type_map_b[ty as usize]);
    }
    module.section(&func_sec);

    // Table Section
    let mut table_sec = wasm_encoder::TableSection::new();
    let mut roundtrip = RoundtripReencoder;
    // Table 0: Module A
    for t in &a.tables {
        table_sec.table(
            roundtrip
                .table_type(*t)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        );
    }
    // Table 1: Module B
    for t in &b.tables {
        table_sec.table(
            roundtrip
                .table_type(*t)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        );
    }
    module.section(&table_sec);

    // Memory Section (shared memory defined by Module A)
    let mut mem_sec = wasm_encoder::MemorySection::new();
    let mut mem_type = roundtrip
        .memory_type(a.memories[0])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    // Ensure at least 32 pages (2MB) for guest runtime data + heap
    if mem_type.minimum < 32 {
        mem_type.minimum = 32;
    }
    mem_sec.memory(mem_type);
    module.section(&mem_sec);

    // Global Section
    let mut global_sec = wasm_encoder::GlobalSection::new();
    // Module A's globals
    for g in &a.globals {
        let gt = roundtrip
            .global_type(g.ty)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let init = reencode_const_expr(&g.init_expr, |idx| idx, |f| func_map_a[f as usize])?;
        global_sec.global(gt, &init);
    }
    // Module B's globals
    for g in &b.globals {
        let gt = roundtrip
            .global_type(g.ty)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let init = reencode_const_expr(
            &g.init_expr,
            |idx| idx + num_a_globals,
            |f| func_map_b[f as usize],
        )?;
        global_sec.global(gt, &init);
    }
    module.section(&global_sec);

    // Export Section
    let mut export_sec = wasm_encoder::ExportSection::new();
    // Exports from Module A
    for exp in &a.exports {
        match exp.kind {
            ExternalKind::Func => {
                let merged_f = func_map_a[exp.index as usize];
                export_sec.export(exp.name, ExportKind::Func, merged_f);
            }
            ExternalKind::Memory => {
                export_sec.export(exp.name, ExportKind::Memory, 0);
            }
            ExternalKind::Table => {
                export_sec.export(exp.name, ExportKind::Table, 0);
            }
            ExternalKind::Global => {
                export_sec.export(exp.name, ExportKind::Global, exp.index);
            }
            _ => (),
        }
    }
    // Exports from Module B (e.g. cabi_realloc, __data_end, __heap_base, runtime methods)
    for exp in &b.exports {
        match exp.kind {
            ExternalKind::Func => {
                let merged_f = func_map_b[exp.index as usize];
                export_sec.export(exp.name, ExportKind::Func, merged_f);
            }
            ExternalKind::Global => {
                export_sec.export(exp.name, ExportKind::Global, exp.index + num_a_globals);
            }
            _ => (),
        }
    }
    module.section(&export_sec);

    // Start Section (from Module B, runs BSS zeroing)
    if let Some(s) = b.start {
        let merged_start = func_map_b[s as usize];
        module.section(&wasm_encoder::StartSection {
            function_index: merged_start,
        });
    }

    // Element Section
    let mut elem_sec = wasm_encoder::ElementSection::new();
    // Elements from Module A (Table 0)
    for el in &a.elements {
        let ElementKind::Active {
            table_index: None | Some(0),
            ref offset_expr,
        } = el.kind
        else {
            bail!("Module A requires active elements targeting Table 0");
        };
        let offset = reencode_const_expr(offset_expr, |idx| idx, |f| func_map_a[f as usize])?;
        let ElementItems::Functions(ref funcs) = el.items else {
            bail!("Module A elements must be function indices");
        };
        let mut remapped_funcs = Vec::new();
        for f in funcs.clone() {
            remapped_funcs.push(func_map_a[f? as usize]);
        }
        elem_sec.segment(ElementSegment {
            mode: ElementMode::Active {
                table: None,
                offset: &offset,
            },
            elements: Elements::Functions(Cow::Owned(remapped_funcs)),
        });
    }
    // Elements from Module B (Table 1)
    for el in &b.elements {
        let ElementKind::Active {
            ref offset_expr, ..
        } = el.kind
        else {
            bail!("Module B requires active elements");
        };
        let offset = reencode_const_expr(
            offset_expr,
            |idx| idx + num_a_globals,
            |f| func_map_b[f as usize],
        )?;
        let ElementItems::Functions(ref funcs) = el.items else {
            bail!("Module B elements must be function indices");
        };
        let mut remapped_funcs = Vec::new();
        for f in funcs.clone() {
            remapped_funcs.push(func_map_b[f? as usize]);
        }
        elem_sec.segment(ElementSegment {
            mode: ElementMode::Active {
                table: Some(1),
                offset: &offset,
            },
            elements: Elements::Functions(Cow::Owned(remapped_funcs)),
        });
    }
    module.section(&elem_sec);

    // Code Section
    let mut code_sec = wasm_encoder::CodeSection::new();
    // Bodies from Module A
    let mut re_a = ReencodeA {
        func_map: &func_map_a,
        type_map: &type_map_a,
    };
    for body in &a.bodies {
        let mut func = re_a
            .new_function_with_parsed_locals(body)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut reader = body.get_operators_reader()?;
        while !reader.eof() {
            let op = re_a
                .parse_instruction(&mut reader)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            func.instruction(&op);
        }
        code_sec.function(&func);
    }
    // Bodies from Module B
    let mut re_b = ReencodeB {
        func_map: &func_map_b,
        type_map: &type_map_b,
        global_offset: num_a_globals,
    };
    for body in &b.bodies {
        let mut func = re_b
            .new_function_with_parsed_locals(body)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut reader = body.get_operators_reader()?;
        while !reader.eof() {
            let op = re_b
                .parse_instruction(&mut reader)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            func.instruction(&op);
        }
        code_sec.function(&func);
    }
    module.section(&code_sec);

    // Data Section
    let mut data_sec = wasm_encoder::DataSection::new();
    // Data from Module A
    for d in &a.data {
        let DataKind::Active {
            memory_index: 0,
            ref offset_expr,
        } = d.kind
        else {
            bail!("Module A data segment must be active on memory 0");
        };
        let offset = reencode_const_expr(offset_expr, |idx| idx, |f| func_map_a[f as usize])?;
        data_sec.segment(DataSegment {
            mode: DataSegmentMode::Active {
                memory_index: 0,
                offset: &offset,
            },
            data: d.data.iter().copied(),
        });
    }
    // Data from Module B
    for d in &b.data {
        let DataKind::Active {
            memory_index: 0,
            ref offset_expr,
        } = d.kind
        else {
            bail!("Module B data segment must be active on memory 0");
        };
        let offset = reencode_const_expr(
            offset_expr,
            |idx| idx + num_a_globals,
            |f| func_map_b[f as usize],
        )?;
        data_sec.segment(DataSegment {
            mode: DataSegmentMode::Active {
                memory_index: 0,
                offset: &offset,
            },
            data: d.data.iter().copied(),
        });
    }
    module.section(&data_sec);

    Ok(module.finish())
}
