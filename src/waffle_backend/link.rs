//! Guest helper linker and relocation.
//!
//! Resolves symbolic helper calls (from module `"__perry_helper"`) in core Wasm
//! emitted by WAFFLE, replacing them with linked and relocated helper functions
//! from embedded `#![no_std]` Rust helper libraries.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{
    CodeSection, ConstExpr, ExportKind, ExportSection, FunctionSection, GlobalSection,
    ImportSection, Module, TypeSection,
};
use wasmparser::{
    ExternalKind, FunctionBody, Parser, Payload, TypeRef, Validator,
};

use super::libraries::{Global, Library, LibraryId, Relocations, SEARCH, TEXT};

pub(crate) const HELPER_MODULE: &str = "__perry_helper";

struct CoreRelocations<'a> {
    functions: &'a BTreeMap<u32, u32>,
}

impl Reencode for CoreRelocations<'_> {
    type Error = String;
    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<String>> {
        self.functions
            .get(&index)
            .copied()
            .ok_or_else(|| reencode::Error::UserError(format!("unmapped core function index {index}")))
    }
}

pub(crate) fn link_helpers(core_wasm: &[u8]) -> Result<Vec<u8>> {
    // 1. First pass: check if any helper imports exist
    let mut has_helper_imports = false;
    for payload in Parser::new(0).parse_all(core_wasm) {
        if let Payload::ImportSection(imports) = payload? {
            for import in imports.into_imports() {
                let import = import?;
                if import.module == HELPER_MODULE {
                    has_helper_imports = true;
                    break;
                }
            }
        }
    }

    if !has_helper_imports {
        return Ok(core_wasm.to_vec());
    }

    // 2. Parse core module sections
    let mut core_types = Vec::new();
    let mut external_imports = Vec::new();
    let mut helper_imports = Vec::new(); // (old_func_idx, entry_name)
    let mut next_import_func_idx = 0u32;

    let mut core_function_type_indices = Vec::new();
    let mut core_bodies: Vec<FunctionBody<'static>> = Vec::new();
    let mut core_exports = Vec::new();
    let mut core_tables = Vec::new();
    let mut core_memories = Vec::new();
    let mut core_globals = Vec::new();
    let mut core_start = None;
    let mut core_elements = Vec::new();
    let mut core_data = Vec::new();

    for payload in Parser::new(0).parse_all(core_wasm) {
        match payload? {
            Payload::TypeSection(types) => {
                for ty in types.into_iter_err_on_gc_types() {
                    core_types.push(ty?);
                }
            }
            Payload::ImportSection(imports) => {
                for import in imports.into_imports() {
                    let import = import?;
                    match import.ty {
                        TypeRef::Func(type_idx) => {
                            let old_func_idx = next_import_func_idx;
                            next_import_func_idx += 1;
                            if import.module == HELPER_MODULE {
                                helper_imports.push((old_func_idx, import.name.to_string(), type_idx));
                            } else {
                                external_imports.push((import.module.to_string(), import.name.to_string(), type_idx));
                            }
                        }
                        _ => {
                            // Non-function imports (if any)
                        }
                    }
                }
            }
            Payload::FunctionSection(funcs) => {
                for ty in funcs {
                    core_function_type_indices.push(ty?);
                }
            }
            Payload::TableSection(tables) => {
                for table in tables {
                    core_tables.push(table?);
                }
            }
            Payload::MemorySection(memories) => {
                for mem in memories {
                    core_memories.push(mem?);
                }
            }
            Payload::GlobalSection(globals) => {
                for glob in globals {
                    core_globals.push(glob?);
                }
            }
            Payload::ExportSection(exports) => {
                for export in exports {
                    core_exports.push(export?);
                }
            }
            Payload::StartSection { func, .. } => {
                core_start = Some(func);
            }
            Payload::ElementSection(elements) => {
                for elem in elements {
                    core_elements.push(elem?);
                }
            }
            Payload::CodeSectionEntry(body) => {
                // Parse body and clone its static bytes
                let binary = body.as_bytes().to_vec().leak();
                core_bodies.push(FunctionBody::new(wasmparser::BinaryReader::new(binary, 0)));
            }
            Payload::DataSection(data) => {
                for seg in data {
                    core_data.push(seg?);
                }
            }
            _ => (),
        }
    }

    let num_new_imported_funcs = external_imports.len() as u32;
    let num_core_defined_funcs = core_function_type_indices.len() as u32;
    let helper_func_start_idx = num_new_imported_funcs + num_core_defined_funcs;

    // 3. Select and prepare helper libraries
    let mut requested_by_lib = BTreeMap::<LibraryId, BTreeSet<String>>::new();
    for (_, entry_name, _) in &helper_imports {
        let lib_id = match entry_name.as_str() {
            "str_find_substring" | "str_scalar_to_byte" => LibraryId::Search,
            "str_code_point_at"
            | "str_from_code_point"
            | "str_case_convert"
            | "str_split_count"
            | "str_split_populate"
            | "str_join_total_len"
            | "str_join" => LibraryId::Text,
            _ => anyhow::bail!("unknown helper entry {entry_name}"),
        };
        requested_by_lib.entry(lib_id).or_default().insert(entry_name.clone());
    }

    let mut old_to_new_func_index = BTreeMap::new();

    // Map external imports
    let mut external_import_idx = 0u32;
    for old_idx in 0..next_import_func_idx {
        if !helper_imports.iter().any(|(h_idx, _, _)| *h_idx == old_idx) {
            old_to_new_func_index.insert(old_idx, external_import_idx);
            external_import_idx += 1;
        }
    }

    // Map core defined functions
    for i in 0..num_core_defined_funcs {
        let old_func_idx = next_import_func_idx + i;
        let new_func_idx = num_new_imported_funcs + i;
        old_to_new_func_index.insert(old_func_idx, new_func_idx);
    }

    // 4. Link helper functions
    let mut new_types = TypeSection::new();
    let mut roundtrip = reencode::RoundtripReencoder;
    for ty in &core_types {
        let params = ty
            .params()
            .iter()
            .map(|ty| roundtrip.val_type(*ty))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let results = ty
            .results()
            .iter()
            .map(|ty| roundtrip.val_type(*ty))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        new_types.ty().function(params, results);
    }

    struct PreparedLibrary {
        lib: Library,
        reachable: BTreeSet<u32>,
        helper_defined_indices: BTreeMap<u32, u32>,
        type_base: u32,
    }

    let mut prepared_libs = Vec::new();
    let mut current_helper_func_idx = helper_func_start_idx;

    for (lib_id, entries) in requested_by_lib {
        let lib_bytes = match lib_id {
            LibraryId::Search => SEARCH,
            LibraryId::Text => TEXT,
        };
        let lib = Library::parse(lib_bytes)?;
        let entry_refs: Vec<&str> = entries.iter().map(|s| s.as_str()).collect();
        let reachable = lib.reachable(&entry_refs)?;

        let type_base = new_types.len();
        for ty in &lib.types {
            let params = ty
                .params()
                .iter()
                .map(|ty| roundtrip.val_type(*ty))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let results = ty
                .results()
                .iter()
                .map(|ty| roundtrip.val_type(*ty))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            new_types.ty().function(params, results);
        }

        let helper_defined_indices = reachable
            .iter()
            .filter(|&&orig| orig >= lib.imports.len() as u32)
            .enumerate()
            .map(|(offset, &orig)| (orig, current_helper_func_idx + offset as u32))
            .collect::<BTreeMap<_, _>>();

        current_helper_func_idx += helper_defined_indices.len() as u32;

        // Map helper entry symbols to their assigned function index
        for entry in &entries {
            let orig_idx = lib.exports.get(entry).context("missing helper export")?;
            let new_idx = helper_defined_indices
                .get(orig_idx)
                .copied()
                .context("missing assigned index for helper entry")?;
            // Map the old core import to this new helper function index!
            for (old_h_idx, h_name, _) in &helper_imports {
                if h_name == entry {
                    old_to_new_func_index.insert(*old_h_idx, new_idx);
                }
            }
        }

        prepared_libs.push(PreparedLibrary {
            lib,
            reachable,
            helper_defined_indices,
            type_base,
        });
    }

    let mut new_functions = FunctionSection::new();
    for &ty_idx in &core_function_type_indices {
        new_functions.function(ty_idx);
    }

    let mut new_code = CodeSection::new();

    // Emit rewritten core function bodies
    let mut core_reloc = CoreRelocations {
        functions: &old_to_new_func_index,
    };
    for body in &core_bodies {
        let mut rewritten = core_reloc
            .new_function_with_parsed_locals(body)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let mut reader = body.get_operators_reader()?;
        while !reader.eof() {
            rewritten.instruction(
                &core_reloc
                    .parse_instruction(&mut reader)
                    .map_err(|error| anyhow::anyhow!("{error}"))?,
            );
        }
        new_code.function(&rewritten);
    }

    let needs_stack = prepared_libs.iter().any(|prep| {
        prep.lib.globals.iter().any(|g| matches!(g, Global::Stack))
    });
    let stack_global_index = core_globals.len() as u32;

    // Emit helper functions
    for prep in prepared_libs {
        let mut relocation = Relocations {
            functions: &prep.helper_defined_indices,
            globals: &prep.lib.globals,
            type_base: prep.type_base,
            memory_base: 0,
            stack_global: stack_global_index,
            table_base: 0,
        };

        for &orig in &prep.reachable {
            if orig < prep.lib.imports.len() as u32 {
                continue;
            }
            let local = orig as usize - prep.lib.imports.len();
            let body = &prep.lib.bodies[local];
            let mut rewritten = relocation
                .new_function_with_parsed_locals(body)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let mut reader = body.get_operators_reader()?;
            while !reader.eof() {
                rewritten.instruction(
                    &relocation
                        .parse_instruction(&mut reader)
                        .map_err(|error| anyhow::anyhow!("{error}"))?,
                );
            }
            new_functions.function(prep.type_base + prep.lib.function_types[local]);
            new_code.function(&rewritten);
        }
    }

    // 5. Construct final linked module
    let mut module = Module::new();
    module.section(&new_types);

    let mut new_imports = ImportSection::new();
    for (module_name, field_name, ty_idx) in external_imports {
        new_imports.import(&module_name, &field_name, wasm_encoder::EntityType::Function(ty_idx));
    }
    if !new_imports.is_empty() {
        module.section(&new_imports);
    }

    module.section(&new_functions);

    // Tables
    if !core_tables.is_empty() {
        let mut new_tables = wasm_encoder::TableSection::new();
        for table in core_tables {
            new_tables.table(wasm_encoder::TableType {
                element_type: match table.ty.element_type {
                    wasmparser::RefType::FUNCREF => wasm_encoder::RefType::FUNCREF,
                    _ => wasm_encoder::RefType::EXTERNREF,
                },
                minimum: table.ty.initial,
                maximum: table.ty.maximum,
                table64: table.ty.table64,
                shared: table.ty.shared,
            });
        }
        module.section(&new_tables);
    }

    // Memories
    if !core_memories.is_empty() {
        let mut new_mems = wasm_encoder::MemorySection::new();
        for mem in core_memories {
            new_mems.memory(wasm_encoder::MemoryType {
                minimum: mem.initial,
                maximum: mem.maximum,
                memory64: mem.memory64,
                shared: mem.shared,
                page_size_log2: mem.page_size_log2,
            });
        }
        module.section(&new_mems);
    }

    // Globals
    if !core_globals.is_empty() || needs_stack {
        let mut new_globs = GlobalSection::new();
        for glob in core_globals {
            let val_type = roundtrip.val_type(glob.ty.content_type).map_err(|e| anyhow::anyhow!("{e}"))?;
            let init = roundtrip.const_expr(glob.init_expr).map_err(|e| anyhow::anyhow!("{e}"))?;
            new_globs.global(
                wasm_encoder::GlobalType {
                    val_type,
                    mutable: glob.ty.mutable,
                    shared: glob.ty.shared,
                },
                &init,
            );
        }
        if needs_stack {
            new_globs.global(
                wasm_encoder::GlobalType {
                    val_type: wasm_encoder::ValType::I32,
                    mutable: true,
                    shared: false,
                },
                &ConstExpr::i32_const(65_536),
            );
        }
        module.section(&new_globs);
    }

    // Exports
    let mut new_exports = ExportSection::new();
    for export in core_exports {
        let kind = match export.kind {
            ExternalKind::Func | ExternalKind::FuncExact => {
                let new_idx = old_to_new_func_index.get(&export.index).copied().unwrap_or(export.index);
                new_exports.export(export.name, ExportKind::Func, new_idx);
                continue;
            }
            ExternalKind::Table => ExportKind::Table,
            ExternalKind::Memory => ExportKind::Memory,
            ExternalKind::Global => ExportKind::Global,
            ExternalKind::Tag => ExportKind::Tag,
        };
        new_exports.export(export.name, kind, export.index);
    }
    module.section(&new_exports);

    // Start
    if let Some(start_func) = core_start {
        let new_start_func = old_to_new_func_index.get(&start_func).copied().unwrap_or(start_func);
        module.section(&wasm_encoder::StartSection {
            function_index: new_start_func,
        });
    }

    // Elements
    if !core_elements.is_empty() {
        let mut new_elems = wasm_encoder::ElementSection::new();
        for elem in core_elements {
            if let wasmparser::ElementItems::Functions(funcs) = elem.items {
                let remapped_funcs: Vec<u32> = funcs
                    .into_iter()
                    .map(|f| {
                        let idx = f?;
                        Ok(old_to_new_func_index.get(&idx).copied().unwrap_or(idx))
                    })
                    .collect::<Result<_, anyhow::Error>>()?;
                if let wasmparser::ElementKind::Active {
                    table_index,
                    offset_expr,
                } = elem.kind
                {
                    let offset = roundtrip
                        .const_expr(offset_expr)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    new_elems.active(
                        table_index,
                        &offset,
                        wasm_encoder::Elements::Functions(std::borrow::Cow::Owned(remapped_funcs)),
                    );
                }
            }
        }
        module.section(&new_elems);
    }

    // Code
    module.section(&new_code);

    // Data
    if !core_data.is_empty() {
        let mut new_data = wasm_encoder::DataSection::new();
        for seg in core_data {
            match seg.kind {
                wasmparser::DataKind::Active { memory_index, offset_expr } => {
                    let offset = roundtrip.const_expr(offset_expr).map_err(|e| anyhow::anyhow!("{e}"))?;
                    new_data.active(memory_index, &offset, seg.data.iter().copied());
                }
                wasmparser::DataKind::Passive => {
                    new_data.passive(seg.data.iter().copied());
                }
            }
        }
        module.section(&new_data);
    }

    let result = module.finish();
    Validator::new().validate_all(&result).context("Validating linked Wasm output")?;
    Ok(result)
}
