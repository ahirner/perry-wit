//! Guest helper linker and relocation.
//!
//! Resolves symbolic helper calls (from module `"__perry_helper"`) in the emitted core Wasm,
//! replacing them with linked and relocated functions from the embedded `#![no_std]` Rust
//! libraries: `perry-runtime` and the computation helpers.

pub(crate) mod raw;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use perry_codegen::HELPER_MODULE;
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{
    CodeSection, ConstExpr, ExportKind, ExportSection, FunctionSection, GlobalSection,
    ImportSection, Module, TypeSection,
};
use wasmparser::{ExternalKind, FunctionBody, Parser, Payload, TypeRef, Validator};

mod data_relocations;
pub(crate) mod libraries;
mod type_compaction;

use libraries::{HelperMemory, Library, LibraryId, Relocations, align_to};
pub(crate) use type_compaction::compact;

struct CoreRelocations<'a> {
    functions: &'a BTreeMap<u32, u32>,
}

impl Reencode for CoreRelocations<'_> {
    type Error = String;
    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<String>> {
        self.functions.get(&index).copied().ok_or_else(|| {
            reencode::Error::UserError(format!("unmapped core function index {index}"))
        })
    }
}

/// Where the canonical built-ins of `stream<u8>` come from: a function of the WIT world that
/// carries one (an import module, or `[export]…` for an export), with the stream's payload
/// index in that function's signature. Every `stream<u8>` of a component instance has the same
/// type, so any such function's built-ins serve all byte streams.
#[derive(Clone, Debug)]
pub(crate) struct StreamAnchor {
    pub(crate) module: String,
    pub(crate) function: String,
    pub(crate) index: u32,
}

/// Placeholder module of byte-stream built-ins in helper libraries (`perry-web`).
const CANON_MODULE: &str = "perry:canon";

impl StreamAnchor {
    /// The concrete import for placeholder `name` (`[async-lower][stream-read]u8`, …).
    fn resolve(&self, name: &str) -> Result<(String, String)> {
        let base = name
            .strip_suffix("u8")
            .with_context(|| format!("unknown canonical placeholder {name}"))?;
        let (prefix, builtin) = match base.strip_prefix("[async-lower]") {
            Some(rest) => ("[async-lower]", rest),
            None => ("", base),
        };
        let builtin = builtin
            .strip_prefix('[')
            .and_then(|b| b.strip_suffix(']'))
            .with_context(|| format!("malformed canonical placeholder {name}"))?;
        Ok((
            self.module.clone(),
            format!("{prefix}[{builtin}-{}]{}", self.index, self.function),
        ))
    }
}

/// [link_helpers_with] without byte streams (helper tests link core modules directly).
#[allow(dead_code)] // Used by the helper tests, which include this module.
pub(crate) fn link_helpers(core_wasm: Vec<u8>) -> Result<Vec<u8>> {
    link_helpers_with(core_wasm, None)
}

/// [link_helpers] for a module of a WIT world whose byte streams use `streams`.
pub(crate) fn link_helpers_with(
    core_wasm: Vec<u8>,
    streams: Option<&StreamAnchor>,
) -> Result<Vec<u8>> {
    // 1. First pass: check if any helper imports exist
    let mut has_helper_imports = false;
    for payload in Parser::new(0).parse_all(&core_wasm) {
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
        return Ok(core_wasm);
    }

    // 2. Parse core module sections
    let mut core_types = Vec::new();
    let mut external_imports = Vec::new();
    let mut helper_imports = Vec::new(); // (old_func_idx, entry_name)
    let mut next_import_func_idx = 0u32;

    let mut core_function_type_indices = Vec::new();
    let mut core_bodies: Vec<FunctionBody<'_>> = Vec::new();
    let mut core_exports = Vec::new();
    let mut core_tables = Vec::new();
    let mut core_memories = Vec::new();
    let mut core_globals = Vec::new();
    let mut core_start = None;
    let mut core_elements = Vec::new();
    let mut core_data = Vec::new();
    let mut core_names = BTreeMap::new();

    for payload in Parser::new(0).parse_all(&core_wasm) {
        match payload? {
            Payload::CustomSection(section) if section.name() == "name" => {
                let names = wasmparser::NameSectionReader::new(section.data_reader());
                for subsection in names {
                    if let wasmparser::Name::Function(names) = subsection? {
                        for name in names {
                            let name = name?;
                            core_names.insert(name.index, name.name);
                        }
                    }
                }
            }
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
                                helper_imports.push((old_func_idx, import.name, type_idx));
                            } else {
                                external_imports.push((
                                    import.module.to_string(),
                                    import.name.to_string(),
                                    type_idx,
                                ));
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
                core_bodies.push(body);
            }
            Payload::DataSection(data) => {
                for seg in data {
                    core_data.push(seg?);
                }
            }
            _ => (),
        }
    }

    let collectible = helper_imports
        .iter()
        .any(|(_, name, _)| name.starts_with("perry_gc_") || name.starts_with("perry_roots_"));

    // 3. Select and prepare helper libraries
    let mut requested_by_lib = BTreeMap::<LibraryId, BTreeSet<String>>::new();
    for (_, entry_name, _) in &helper_imports {
        let lib_id = LibraryId::for_entry(entry_name)?;
        requested_by_lib
            .entry(lib_id)
            .or_default()
            .insert(entry_name.to_string());
    }
    // Libraries may call each other through `__perry_helper` imports (the runtime formats
    // numbers with the number helper): request those entries too, to a fixpoint.
    loop {
        let mut added = Vec::new();
        for (lib_id, entries) in &requested_by_lib {
            let lib = Library::parse(lib_id.for_memory(collectible))?;
            let reachable = lib.reachable(entries.iter().map(String::as_str))?;
            for &orig in reachable
                .iter()
                .filter(|&&orig| orig < lib.imports.len() as u32)
            {
                let (module, name, _) = &lib.imports[orig as usize];
                if module == HELPER_MODULE {
                    added.push((LibraryId::for_entry(name)?, name.clone()));
                }
            }
        }
        let mut changed = false;
        for (lib_id, name) in added {
            changed |= requested_by_lib.entry(lib_id).or_default().insert(name);
        }
        if !changed {
            break;
        }
    }

    let mut new_types = TypeSection::new();
    let mut roundtrip = reencode::RoundtripReencoder;
    let mut push_type = |types: &mut TypeSection, ty: &wasmparser::FuncType| -> Result<()> {
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
        types.ty().function(params, results);
        Ok(())
    };
    for ty in &core_types {
        push_type(&mut new_types, ty)?;
    }

    struct PreparedLibrary {
        id: LibraryId,
        lib: Library,
        reachable: BTreeSet<u32>,
        helper_defined_indices: BTreeMap<u32, u32>,
        type_base: u32,
        entries: BTreeSet<String>,
    }

    let mut prepared_libs = Vec::new();
    for (lib_id, entries) in requested_by_lib {
        let lib = Library::parse(lib_id.for_memory(collectible))?;
        let reachable = lib.reachable(entries.iter().map(String::as_str))?;
        let type_base = new_types.len();
        for ty in &lib.types {
            push_type(&mut new_types, ty)?;
        }
        // Reachable imports of the library join the module's imports (shared by name).
        let mut helper_defined_indices = BTreeMap::new();
        for &orig in reachable
            .iter()
            .filter(|&&orig| orig < lib.imports.len() as u32)
        {
            let (module, name, ty) = &lib.imports[orig as usize];
            if module == HELPER_MODULE {
                // Resolved to the other library's function once indices are assigned.
                continue;
            }
            let (module, name) = if module == CANON_MODULE {
                streams
                    .context("byte streams need a WIT function with a stream<u8> in the world")?
                    .resolve(name)?
            } else {
                (module.clone(), name.clone())
            };
            let (module, name) = (&module, &name);
            let index = match external_imports
                .iter()
                .position(|(m, n, _)| m == module && n == name)
            {
                Some(index) => index,
                None => {
                    external_imports.push((module.clone(), name.clone(), type_base + ty));
                    external_imports.len() - 1
                }
            };
            helper_defined_indices.insert(orig, index as u32);
        }
        prepared_libs.push(PreparedLibrary {
            id: lib_id,
            lib,
            reachable,
            helper_defined_indices,
            type_base,
            entries,
        });
    }

    let num_new_imported_funcs = external_imports.len() as u32;
    let num_core_defined_funcs = core_function_type_indices.len() as u32;
    let helper_func_start_idx = num_new_imported_funcs + num_core_defined_funcs;

    let mut old_to_new_func_index = BTreeMap::new();

    // Map external imports of the core module (they keep their order, first).
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

    // 4. Assign indices to the selected helper functions
    let mut current_helper_func_idx = helper_func_start_idx;
    let mut entry_index = BTreeMap::<(LibraryId, String), u32>::new();
    for prep in &mut prepared_libs {
        let imported = prep.lib.imports.len() as u32;
        for &orig in prep.reachable.iter().filter(|&&orig| orig >= imported) {
            prep.helper_defined_indices
                .insert(orig, current_helper_func_idx);
            current_helper_func_idx += 1;
        }
        for entry in &prep.entries {
            let orig_idx = prep
                .lib
                .exports
                .get(entry.as_str())
                .context("missing helper export")?;
            let new_idx = prep
                .helper_defined_indices
                .get(orig_idx)
                .copied()
                .context("missing assigned index for helper entry")?;
            entry_index.insert((prep.id, entry.clone()), new_idx);
            for (old_h_idx, h_name, _) in &helper_imports {
                if h_name == entry {
                    old_to_new_func_index.insert(*old_h_idx, new_idx);
                }
            }
        }
    }
    // Calls between libraries.
    for prep in &mut prepared_libs {
        for &orig in prep
            .reachable
            .iter()
            .filter(|&&orig| orig < prep.lib.imports.len() as u32)
        {
            let (module, name, _) = &prep.lib.imports[orig as usize];
            if module == HELPER_MODULE {
                let target = entry_index
                    .get(&(LibraryId::for_entry(name)?, name.clone()))
                    .copied()
                    .context("missing helper entry for a library import")?;
                prep.helper_defined_indices.insert(orig, target);
            }
        }
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

    // Determine existing static data extent from core module
    let mut existing_data_end = 1024u32;
    for seg in &core_data {
        if let wasmparser::DataKind::Active { offset_expr, .. } = &seg.kind {
            let mut reader = offset_expr.get_operators_reader();
            if let Ok(wasmparser::Operator::I32Const { value }) = reader.read() {
                let end = (value as u32).saturating_add(seg.data.len() as u32);
                existing_data_end = existing_data_end.max(end);
            }
        }
    }

    // Allocate memory and table placements for each helper library
    struct LibPlacement {
        memory_base: u32,
        table_base: u32,
    }
    let mut helper_memory = HelperMemory::new(existing_data_end);
    let mut current_table_size = if !core_tables.is_empty() {
        core_tables[0].ty.initial as u32
    } else {
        0
    };
    let mut lib_placements = Vec::new();
    for prep in &prepared_libs {
        let memory_base = helper_memory.place(&prep.lib, Some(&prep.reachable))?;

        let table_base = align_to(current_table_size, prep.lib.table_alignment)?;
        current_table_size = table_base
            .checked_add(prep.lib.table_size)
            .context("helper table exceeds table32")?;

        lib_placements.push(LibPlacement {
            memory_base,
            table_base,
        });
    }

    let needs_stack = helper_memory.needs_stack;
    let stack_global_index = core_globals.len() as u32;
    let stack_top = helper_memory.stack_top()?;

    if needs_stack && !core_memories.is_empty() {
        let needed_pages = stack_top.div_ceil(65_536) as u64;
        if core_memories[0].initial < needed_pages {
            core_memories[0].initial = needed_pages;
        }
    }

    // Emit helper functions
    for (prep, placement) in prepared_libs.iter().zip(lib_placements.iter()) {
        let mut relocation = Relocations {
            functions: &prep.helper_defined_indices,
            globals: &prep.lib.globals,
            type_base: prep.type_base,
            memory_base: placement.memory_base,
            stack_global: stack_global_index,
            table_base: placement.table_base,
            heap_base: stack_top,
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

    let folded: Vec<_> = prepared_libs
        .iter()
        .zip(&lib_placements)
        .map(|(prep, placement)| {
            data_relocations::fold(
                &prep.lib,
                placement.memory_base,
                placement.table_base,
                stack_top,
            )
        })
        .collect();

    // Helper initialization
    let mut helper_inits = Vec::new();
    for (prep, folded) in prepared_libs.iter().zip(&folded) {
        for orig in prep.lib.initializers() {
            if folded.as_ref().is_some_and(|data| data.function == orig) {
                continue;
            }
            if let Some(&new_idx) = prep.helper_defined_indices.get(&orig)
                && !helper_inits.contains(&new_idx)
            {
                helper_inits.push(new_idx);
            }
        }
    }

    let init_func_idx = if !helper_inits.is_empty() {
        let empty_ty_idx = new_types.len();
        new_types.ty().function(vec![], vec![]);
        new_functions.function(empty_ty_idx);
        let mut init_fn = wasm_encoder::Function::new(vec![]);
        for &fn_idx in &helper_inits {
            init_fn.instruction(&wasm_encoder::Instruction::Call(fn_idx));
        }
        if let Some(start_func) = core_start {
            let new_start_func = old_to_new_func_index
                .get(&start_func)
                .copied()
                .unwrap_or(start_func);
            init_fn.instruction(&wasm_encoder::Instruction::Call(new_start_func));
            core_start = None;
        }
        init_fn.instruction(&wasm_encoder::Instruction::End);
        new_code.function(&init_fn);
        let total_funcs = external_imports.len() as u32 + new_functions.len();
        Some(total_funcs - 1)
    } else {
        None
    };

    // 5. Construct final linked module
    let mut module = Module::new();
    module.section(&new_types);

    let mut new_imports = ImportSection::new();
    for (module_name, field_name, ty_idx) in &external_imports {
        new_imports.import(
            module_name,
            field_name,
            wasm_encoder::EntityType::Function(*ty_idx),
        );
    }
    if !new_imports.is_empty() {
        module.section(&new_imports);
    }

    module.section(&new_functions);

    // Tables
    let has_helper_tables = current_table_size as u64
        > (if !core_tables.is_empty() {
            core_tables[0].ty.initial
        } else {
            0
        });
    if !core_tables.is_empty() || has_helper_tables {
        let mut new_tables = wasm_encoder::TableSection::new();
        if !core_tables.is_empty() {
            for (idx, table) in core_tables.iter().enumerate() {
                let initial = if idx == 0 {
                    table.ty.initial.max(current_table_size as u64)
                } else {
                    table.ty.initial
                };
                let maximum = table
                    .ty
                    .maximum
                    .map(|maximum| {
                        maximum
                            .checked_add(initial - table.ty.initial)
                            .filter(|maximum| table.ty.table64 || *maximum <= u64::from(u32::MAX))
                            .context("Helper function table exceeds its address space")
                    })
                    .transpose()?;
                new_tables.table(wasm_encoder::TableType {
                    element_type: match table.ty.element_type {
                        wasmparser::RefType::FUNCREF => wasm_encoder::RefType::FUNCREF,
                        _ => wasm_encoder::RefType::EXTERNREF,
                    },
                    minimum: initial,
                    maximum,
                    table64: table.ty.table64,
                    shared: table.ty.shared,
                });
            }
        } else if has_helper_tables {
            new_tables.table(wasm_encoder::TableType {
                element_type: wasm_encoder::RefType::FUNCREF,
                minimum: current_table_size as u64,
                maximum: None,
                table64: false,
                shared: false,
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
            let val_type = roundtrip
                .val_type(glob.ty.content_type)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let init = roundtrip
                .const_expr(glob.init_expr)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
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
                &ConstExpr::i32_const(stack_top as i32),
            );
        }
        module.section(&new_globs);
    }

    // Exports
    let mut new_exports = ExportSection::new();
    for export in core_exports {
        let kind = match export.kind {
            ExternalKind::Func | ExternalKind::FuncExact => {
                let new_idx = old_to_new_func_index
                    .get(&export.index)
                    .copied()
                    .unwrap_or(export.index);
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
    if let Some(init_idx) = init_func_idx {
        module.section(&wasm_encoder::StartSection {
            function_index: init_idx,
        });
    } else if let Some(start_func) = core_start {
        let new_start_func = old_to_new_func_index
            .get(&start_func)
            .copied()
            .unwrap_or(start_func);
        module.section(&wasm_encoder::StartSection {
            function_index: new_start_func,
        });
    }

    // Elements
    let mut new_elems = wasm_encoder::ElementSection::new();
    if !core_elements.is_empty() {
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
    }
    for (prep, placement) in prepared_libs.iter().zip(lib_placements.iter()) {
        for &(slot, orig_func) in &prep.lib.elements {
            if let Some(&target) = prep.helper_defined_indices.get(&orig_func) {
                new_elems.active(
                    Some(0),
                    &ConstExpr::i32_const((placement.table_base + slot) as i32),
                    wasm_encoder::Elements::Functions(std::borrow::Cow::Owned(vec![target])),
                );
            }
        }
    }
    if !new_elems.is_empty() {
        module.section(&new_elems);
    }

    // Code
    module.section(&new_code);

    // Data
    let mut new_data = wasm_encoder::DataSection::new();
    if !core_data.is_empty() {
        for seg in core_data {
            match seg.kind {
                wasmparser::DataKind::Active {
                    memory_index,
                    offset_expr,
                } => {
                    let offset = roundtrip
                        .const_expr(offset_expr)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    new_data.active(memory_index, &offset, seg.data.iter().copied());
                }
                wasmparser::DataKind::Passive => {
                    new_data.passive(seg.data.iter().copied());
                }
            }
        }
    }
    for ((prep, placement), folded) in prepared_libs.iter().zip(&lib_placements).zip(&folded) {
        let mut append = |offset: u32, contents: &[u8]| {
            new_data.active(
                0,
                &ConstExpr::i32_const((placement.memory_base + offset) as i32),
                contents.iter().copied(),
            );
        };
        if let Some(folded) = folded {
            for (offset, contents) in &folded.segments {
                append(*offset, contents);
            }
        } else {
            for &(offset, contents) in &prep.lib.data {
                append(offset, contents);
            }
        }
    }
    if !new_data.is_empty() {
        module.section(&new_data);
    }

    let mut functions = BTreeMap::new();
    for (index, (_, name, _)) in external_imports.iter().enumerate() {
        functions.insert(index as u32, name.clone());
    }
    for (old, name) in core_names {
        if let Some(&index) = old_to_new_func_index.get(&old) {
            functions.insert(index, name.to_owned());
        }
    }
    for prep in &prepared_libs {
        for (&original, &index) in &prep.helper_defined_indices {
            if original >= prep.lib.imports.len() as u32 {
                functions.insert(index, format!("{:?}::{original}", prep.id));
            }
        }
        for (name, original) in &prep.lib.exports {
            if let Some(&index) = prep.helper_defined_indices.get(original) {
                functions.insert(index, name.clone());
            }
        }
    }
    let mut names = wasm_encoder::NameMap::new();
    for (index, name) in functions {
        names.append(index, &name);
    }
    let mut section = wasm_encoder::NameSection::new();
    section.functions(&names);
    module.section(&section);

    let result = module.finish();
    if let Err(e) = Validator::new().validate_all(&result) {
        std::fs::write("/tmp/debug_linked.wasm", &result).ok();
        anyhow::bail!("Validating linked Wasm output: {e:?} (wrote /tmp/debug_linked.wasm)");
    }
    Ok(result)
}
