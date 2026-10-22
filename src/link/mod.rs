//! Guest helper linker and relocation.
//!
//! Resolves symbolic helper calls (from module `"__perry_helper"`) in the emitted core Wasm,
//! replacing them with linked and relocated functions from the embedded `#![no_std]` Rust
//! libraries: `perry-runtime` and the computation helpers.

pub(crate) mod raw;

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use anyhow::Result;
use parsed::ParsedModule;
use perry_codegen::HELPER_MODULE;
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{
    CodeSection, ConstExpr, ExportKind, ExportSection, FunctionSection, GlobalSection,
    ImportSection, Module, TypeSection,
};
use wasmparser::Validator;

pub(crate) mod canonical;
mod data_relocations;
pub(crate) mod libraries;
mod parsed;
#[cfg(test)]
mod tests;
mod type_compaction;

use libraries::{HelperMemory, Library, LibraryId, Relocations, align_to};
pub(crate) use type_compaction::compact;

#[derive(Debug)]
pub(crate) enum LinkError {
    UnknownCanonicalPlaceholder(String),
    MalformedCanonicalPlaceholder(String),
    UnsupportedCoreImports,
    MissingStreamAnchor,
    TableOverflow(&'static str),
    MissingHelperExport(String),
    MissingHelperIndex(String),
    Reencode(String),
    Validation(wasmparser::BinaryReaderError),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCanonicalPlaceholder(name) => {
                write!(f, "unknown canonical placeholder {name}")
            }
            Self::MalformedCanonicalPlaceholder(name) => {
                write!(f, "malformed canonical placeholder {name}")
            }
            Self::UnsupportedCoreImports => {
                f.write_str("core helper linking requires defined memory, tables and globals")
            }
            Self::MissingStreamAnchor => {
                f.write_str("byte streams need a WIT function with a stream<u8> in the world")
            }
            Self::TableOverflow(msg) => f.write_str(msg),
            Self::MissingHelperExport(entry) => write!(f, "missing helper export {entry}"),
            Self::MissingHelperIndex(entry) => {
                write!(f, "missing assigned index for helper entry {entry}")
            }
            Self::Reencode(msg) => f.write_str(msg),
            Self::Validation(err) => write!(f, "Validating linked Wasm output: {err:?}"),
        }
    }
}

impl std::error::Error for LinkError {}

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
    fn resolve(&self, name: &str) -> Result<(String, String), LinkError> {
        let base = name
            .strip_suffix("u8")
            .ok_or_else(|| LinkError::UnknownCanonicalPlaceholder(name.to_owned()))?;
        let (prefix, builtin) = match base.strip_prefix("[async-lower]") {
            Some(rest) => ("[async-lower]", rest),
            None => ("", base),
        };
        let builtin = builtin
            .strip_prefix('[')
            .and_then(|b| b.strip_suffix(']'))
            .ok_or_else(|| LinkError::MalformedCanonicalPlaceholder(name.to_owned()))?;
        Ok((
            self.module.clone(),
            format!("{prefix}[{builtin}-{}]{}", self.index, self.function),
        ))
    }
}

/// Link `perry:helper/*` imports for standalone core-module tests.
#[cfg(test)]
fn link_helpers(core_wasm: Vec<u8>) -> Result<Vec<u8>> {
    Ok(link_helpers_with(core_wasm, None, &canonical::Requirements::default())?.0)
}

/// Link `perry:helper/*` imports and rewrite canonical stream placeholders for `core_wasm`.
pub(crate) fn link_helpers_with(
    core_wasm: Vec<u8>,
    streams: Option<&StreamAnchor>,
    canonical: &canonical::Requirements,
) -> Result<(Vec<u8>, u32)> {
    let mut core = ParsedModule::parse(&core_wasm)?;
    let mut export_realloc = canonical.export_realloc;
    let mut helper_imports = Vec::new();
    let mut external_imports = Vec::new();
    let mut old_to_new_func_index = BTreeMap::new();
    for (index, &(module, name, ty)) in core.imports.iter().enumerate() {
        export_realloc |= canonical.import(module, name);
        if module == HELPER_MODULE {
            helper_imports.push((index as u32, name, ty));
        } else {
            old_to_new_func_index.insert(index as u32, external_imports.len() as u32);
            external_imports.push((Cow::Borrowed(module), Cow::Borrowed(name), ty));
        }
    }
    if helper_imports.is_empty() && !export_realloc {
        return Ok((core_wasm, 0));
    }
    if !core.other_imports.is_empty() {
        return Err(LinkError::UnsupportedCoreImports.into());
    }
    let next_import_func_idx = core.imports.len() as u32;

    let collectible = helper_imports
        .iter()
        .any(|(_, name, _)| name.starts_with("perry_gc_") || name.starts_with("perry_roots_"));

    struct PreparedLibrary<'a> {
        id: LibraryId,
        lib: Library<'static>,
        reachable: BTreeSet<u32>,
        helper_defined_indices: BTreeMap<u32, u32>,
        type_base: u32,
        entries: BTreeSet<&'a str>,
    }

    let mut new_types = TypeSection::new();
    for ty in &core.types {
        new_types.ty().func_type(ty);
    }
    let mut pending = helper_imports
        .iter()
        .map(|(_, entry, _)| Ok((LibraryId::for_entry(entry)?, *entry)))
        .collect::<Result<BTreeSet<_>>>()?;
    if export_realloc {
        pending.insert((LibraryId::Runtime, "perry_cabi_realloc"));
    }
    let mut selected = BTreeMap::new();
    while let Some((id, entry)) = pending.pop_first() {
        let prep = match selected.entry(id) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let lib = Library::parse(id.for_memory(collectible))?;
                let type_base = new_types.len();
                for ty in &lib.module.types {
                    new_types.ty().func_type(ty);
                }
                entry.insert(PreparedLibrary {
                    id,
                    lib,
                    type_base,
                    reachable: BTreeSet::new(),
                    helper_defined_indices: BTreeMap::new(),
                    entries: BTreeSet::new(),
                })
            }
        };
        if !prep.entries.insert(entry) {
            continue;
        }
        let reachable = prep
            .lib
            .reachable([entry], &BTreeSet::new(), &prep.reachable)?;
        for &orig in reachable.range(..prep.lib.module.imports.len() as u32) {
            let (module, name, ty) = prep.lib.module.imports[orig as usize];
            if module == HELPER_MODULE {
                pending.insert((LibraryId::for_entry(name)?, name));
                continue;
            }
            if !export_realloc && canonical.import(module, name) {
                export_realloc = true;
                pending.insert((LibraryId::Runtime, "perry_cabi_realloc"));
            }
            let (module, name) = if module == CANON_MODULE {
                let (module, name) = streams
                    .ok_or(LinkError::MissingStreamAnchor)?
                    .resolve(name)?;
                (Cow::Owned(module), Cow::Owned(name))
            } else {
                (Cow::Borrowed(module), Cow::Borrowed(name))
            };
            let index = match external_imports
                .iter()
                .position(|(m, n, _)| m == &module && n == &name)
            {
                Some(index) => index,
                None => {
                    external_imports.push((module, name, prep.type_base + ty));
                    external_imports.len() - 1
                }
            };
            prep.helper_defined_indices.insert(orig, index as u32);
        }
        prep.reachable.extend(reachable);
    }
    let mut prepared_libs: Vec<_> = selected.into_values().collect();

    let num_new_imported_funcs = external_imports.len() as u32;
    let num_core_defined_funcs = core.function_types.len() as u32;
    let helper_func_start_idx = num_new_imported_funcs + num_core_defined_funcs;

    // Map core defined functions
    for i in 0..num_core_defined_funcs {
        let old_func_idx = next_import_func_idx + i;
        let new_func_idx = num_new_imported_funcs + i;
        old_to_new_func_index.insert(old_func_idx, new_func_idx);
    }

    // Determine existing static data extent from core module
    let mut existing_data_end = 1024u32;
    for seg in core.data.iter().cloned().flatten() {
        let seg = seg?;
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
    let original_table_size = core
        .tables
        .clone()
        .into_iter()
        .flatten()
        .next()
        .transpose()?
        .map_or(0, |table| table.ty.initial);
    let mut current_table_size = u32::try_from(original_table_size)
        .map_err(|_| LinkError::TableOverflow("core table exceeds table32"))?;
    let mut lib_placements = Vec::new();
    for prep in &prepared_libs {
        let memory_base = helper_memory.place(&prep.lib, Some(&prep.reachable))?;

        let table_base = align_to(current_table_size, prep.lib.table_alignment)?;
        current_table_size = table_base
            .checked_add(prep.lib.table_size)
            .ok_or(LinkError::TableOverflow("helper table exceeds table32"))?;

        lib_placements.push(LibPlacement {
            memory_base,
            table_base,
        });
    }

    let needs_stack = helper_memory.needs_stack;
    let stack_global_index = core.globals.as_ref().map_or(0, |section| section.count());
    let stack_top = helper_memory.stack_top()?;

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

    for (prep, folded) in prepared_libs.iter_mut().zip(&folded) {
        if let Some(folded) = folded {
            prep.reachable = prep.lib.reachable(
                prep.entries.iter().copied(),
                &folded.functions,
                &BTreeSet::new(),
            )?;
        }
    }

    // 4. Assign indices to the selected helper functions
    let mut current_helper_func_idx = helper_func_start_idx;
    let mut entry_index = BTreeMap::<(LibraryId, &str), u32>::new();
    for prep in &mut prepared_libs {
        let imported = prep.lib.module.imports.len() as u32;
        for &orig in prep.reachable.iter().filter(|&&orig| orig >= imported) {
            prep.helper_defined_indices
                .insert(orig, current_helper_func_idx);
            current_helper_func_idx += 1;
        }
        for entry in &prep.entries {
            let orig_idx = prep
                .lib
                .exports
                .get(entry)
                .ok_or_else(|| LinkError::MissingHelperExport((*entry).to_owned()))?;
            let new_idx = prep
                .helper_defined_indices
                .get(orig_idx)
                .copied()
                .ok_or_else(|| LinkError::MissingHelperIndex((*entry).to_owned()))?;
            entry_index.insert((prep.id, *entry), new_idx);
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
            .filter(|&&orig| orig < prep.lib.module.imports.len() as u32)
        {
            let (module, name, _) = prep.lib.module.imports[orig as usize];
            if module == HELPER_MODULE {
                let target = entry_index
                    .get(&(LibraryId::for_entry(name)?, name))
                    .copied()
                    .ok_or_else(|| LinkError::MissingHelperIndex(name.to_owned()))?;
                prep.helper_defined_indices.insert(orig, target);
            }
        }
    }

    let mut new_functions = FunctionSection::new();
    let mut new_code = CodeSection::new();
    let mut core_reloc = CoreRelocations {
        functions: &old_to_new_func_index,
    };
    core.emit_functions(
        &mut new_functions,
        &mut new_code,
        0..core.bodies.len(),
        &mut core_reloc,
    )?;
    for (prep, placement) in prepared_libs.iter().zip(&lib_placements) {
        let mut relocation = Relocations {
            functions: &prep.helper_defined_indices,
            globals: &prep.lib.globals,
            type_base: prep.type_base,
            memory_base: placement.memory_base,
            stack_global: stack_global_index,
            table_base: placement.table_base,
            heap_base: stack_top,
        };
        let imported = prep.lib.module.imports.len() as u32;
        let selected = prep
            .reachable
            .iter()
            .filter_map(|index| index.checked_sub(imported).map(|index| index as usize));
        prep.lib.module.emit_functions(
            &mut new_functions,
            &mut new_code,
            selected,
            &mut relocation,
        )?;
    }

    // Helper initialization
    let mut helper_inits = Vec::new();
    for (prep, folded) in prepared_libs.iter().zip(&folded) {
        for orig in prep.lib.initializers() {
            if folded
                .as_ref()
                .is_some_and(|data| data.functions.contains(&orig))
            {
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
        if let Some(start_func) = core.start {
            let new_start_func = old_to_new_func_index
                .get(&start_func)
                .copied()
                .unwrap_or(start_func);
            init_fn.instruction(&wasm_encoder::Instruction::Call(new_start_func));
            core.start = None;
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

    let mut new_tables = wasm_encoder::TableSection::new();
    for (index, table) in core.tables.iter().cloned().flatten().enumerate() {
        let mut table = table?;
        if index == 0 {
            let initial = table.ty.initial.max(u64::from(current_table_size));
            table.ty.maximum = table
                .ty
                .maximum
                .map(|maximum| {
                    maximum
                        .checked_add(initial - table.ty.initial)
                        .filter(|maximum| table.ty.table64 || *maximum <= u64::from(u32::MAX))
                        .ok_or(LinkError::TableOverflow(
                            "Helper function table exceeds its address space",
                        ))
                })
                .transpose()?;
            table.ty.initial = initial;
        }
        core_reloc
            .parse_table(&mut new_tables, table)
            .map_err(|error| LinkError::Reencode(error.to_string()))?;
    }
    if new_tables.is_empty() && current_table_size != 0 {
        new_tables.table(wasm_encoder::TableType {
            element_type: wasm_encoder::RefType::FUNCREF,
            minimum: u64::from(current_table_size),
            maximum: None,
            table64: false,
            shared: false,
        });
    }
    if !new_tables.is_empty() {
        module.section(&new_tables);
    }

    if let Some(memories) = core.memories.take() {
        let mut new_mems = wasm_encoder::MemorySection::new();
        for (index, memory) in memories.into_iter().enumerate() {
            let mut memory = memory?;
            if index == 0 && needs_stack {
                memory.initial = memory.initial.max(u64::from(stack_top.div_ceil(65_536)));
            }
            new_mems.memory(
                core_reloc
                    .memory_type(memory)
                    .map_err(|error| LinkError::Reencode(error.to_string()))?,
            );
        }
        module.section(&new_mems);
    }
    if let Some(tags) = core.tags.take() {
        let mut new_tags = wasm_encoder::TagSection::new();
        core_reloc
            .parse_tag_section(&mut new_tags, tags)
            .map_err(|error| LinkError::Reencode(error.to_string()))?;
        module.section(&new_tags);
    }
    let mut new_globs = GlobalSection::new();
    if let Some(globals) = core.globals.take() {
        core_reloc
            .parse_global_section(&mut new_globs, globals)
            .map_err(|error| LinkError::Reencode(error.to_string()))?;
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
    if !new_globs.is_empty() {
        module.section(&new_globs);
    }

    // Exports
    let mut new_exports = ExportSection::new();
    if let Some(exports) = core.exports.take() {
        core_reloc
            .parse_export_section(&mut new_exports, exports)
            .map_err(|error| LinkError::Reencode(error.to_string()))?;
    }
    if export_realloc {
        new_exports.export(
            "cabi_realloc",
            ExportKind::Func,
            entry_index[&(LibraryId::Runtime, "perry_cabi_realloc")],
        );
    }
    module.section(&new_exports);

    // Start
    if let Some(init_idx) = init_func_idx {
        module.section(&wasm_encoder::StartSection {
            function_index: init_idx,
        });
    } else if let Some(start_func) = core.start {
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
    if let Some(elements) = core.elements.take() {
        core_reloc
            .parse_element_section(&mut new_elems, elements)
            .map_err(|error| LinkError::Reencode(error.to_string()))?;
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

    // Data
    let mut new_data = wasm_encoder::DataSection::new();
    if let Some(data) = core.data.take() {
        core_reloc
            .parse_data_section(&mut new_data, data)
            .map_err(|error| LinkError::Reencode(error.to_string()))?;
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
    if core.data_count.is_some() {
        module.section(&wasm_encoder::DataCountSection {
            count: new_data.len(),
        });
    }
    module.section(&new_code);
    if !new_data.is_empty() {
        module.section(&new_data);
    }

    let mut functions = BTreeMap::new();
    for (index, (_, name, _)) in external_imports.iter().enumerate() {
        functions.insert(index as u32, name.clone());
    }
    for section in &core.custom {
        if section.name() == "name" {
            for subsection in wasmparser::NameSectionReader::new(section.data_reader()) {
                if let wasmparser::Name::Function(names) = subsection? {
                    for name in names {
                        let name = name?;
                        if let Some(&index) = old_to_new_func_index.get(&name.index) {
                            functions.insert(index, Cow::Borrowed(name.name));
                        }
                    }
                }
            }
        }
    }
    for prep in &prepared_libs {
        for (&original, &index) in &prep.helper_defined_indices {
            if original >= prep.lib.module.imports.len() as u32 {
                functions.insert(index, Cow::Owned(format!("{:?}::{original}", prep.id)));
            }
        }
        for (name, original) in &prep.lib.exports {
            if let Some(&index) = prep.helper_defined_indices.get(original) {
                functions.insert(index, Cow::Borrowed(*name));
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
    Validator::new()
        .validate_all(&result)
        .map_err(LinkError::Validation)?;
    Ok((result, current_helper_func_idx - helper_func_start_idx))
}
