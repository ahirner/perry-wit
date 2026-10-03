//! Guest helper libraries: parsing, reachability analysis, and relocations.
//!
//! Provides compiled helper artifacts compiled to `wasm32-unknown-unknown`
//! without runtime dependencies on rustc/LLVM.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use wasm_encoder::Instruction;
use wasm_encoder::reencode::{self, Reencode};
use wasmparser::{
    Dylink0Subsection, ElementItems, ElementKind, ExternalKind, Operator, Parser, Payload, TypeRef,
};

pub(crate) const SEARCH: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/search.wasm"));
pub(crate) const TEXT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.wasm"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LibraryId {
    Search,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Global {
    Stack,
    MemoryBase,
    TableBase,
    Constant(i32),
}

pub(crate) struct Library {
    pub(crate) types: Vec<wasmparser::FuncType>,
    pub(crate) function_types: Vec<u32>,
    pub(crate) bodies: Vec<wasmparser::FunctionBody<'static>>,
    pub(crate) exports: BTreeMap<String, u32>,
    pub(crate) globals: Vec<Global>,
    pub(crate) data: Vec<(u32, &'static [u8])>,
    pub(crate) data_size: u32,
    pub(crate) data_alignment: u32,
    pub(crate) imports: Vec<(String, u32)>,
    pub(crate) elements: Vec<(u32, u32)>,
    pub(crate) table_size: u32,
    pub(crate) table_alignment: u32,
    pub(crate) start: Option<u32>,
}

impl Library {
    pub(crate) fn parse(bytes: &'static [u8]) -> Result<Self> {
        let mut library = Self {
            types: vec![],
            function_types: vec![],
            bodies: vec![],
            exports: BTreeMap::new(),
            globals: vec![],
            data: vec![],
            data_size: 0,
            data_alignment: 1,
            imports: vec![],
            elements: vec![],
            table_size: 0,
            table_alignment: 1,
            start: None,
        };
        for payload in Parser::new(0).parse_all(bytes) {
            match payload? {
                Payload::TypeSection(types) => {
                    library.types = types
                        .into_iter_err_on_gc_types()
                        .collect::<Result<_, _>>()?;
                }
                Payload::ImportSection(imports) => {
                    for import in imports.into_imports() {
                        let import = import?;
                        match (import.module, import.name, import.ty) {
                            ("env", "memory", TypeRef::Memory(memory)) => {
                                ensure!(
                                    !memory.memory64 && !memory.shared,
                                    "internal helpers use ordinary 32-bit application memory"
                                );
                            }
                            ("env", "__indirect_function_table", TypeRef::Table(table)) => {
                                ensure!(
                                    !table.table64
                                        && !table.shared
                                        && table.element_type == wasmparser::RefType::FUNCREF,
                                    "internal helper tables contain ordinary function references"
                                );
                            }
                            ("env", name, TypeRef::Global(global)) => {
                                ensure!(
                                    global.content_type == wasmparser::ValType::I32,
                                    "internal helper globals must be i32"
                                );
                                library.globals.push(match name {
                                    "__stack_pointer" if global.mutable => Global::Stack,
                                    "__memory_base" if !global.mutable => Global::MemoryBase,
                                    "__table_base" if !global.mutable => Global::TableBase,
                                    _ => bail!("unsupported internal helper global {name}"),
                                });
                            }
                            _ => bail!(
                                "unsupported helper import {}.{}",
                                import.module,
                                import.name
                            ),
                        }
                    }
                }
                Payload::FunctionSection(functions) => {
                    library.function_types = functions.into_iter().collect::<Result<_, _>>()?;
                }
                Payload::ExportSection(exports) => {
                    for export in exports {
                        let export = export?;
                        if export.kind == ExternalKind::Func {
                            library
                                .exports
                                .insert(export.name.to_string(), export.index);
                        }
                    }
                }
                Payload::CodeSectionEntry(body) => library.bodies.push(body),
                Payload::DataSection(data) => {
                    for segment in data {
                        let segment = segment?;
                        let wasmparser::DataKind::Active {
                            memory_index: 0,
                            offset_expr,
                        } = segment.kind
                        else {
                            bail!("internal helpers require active data in application memory");
                        };
                        let mut reader = offset_expr.get_operators_reader();
                        let mut stack = Vec::<u32>::new();
                        while !reader.eof() {
                            match reader.read()? {
                                Operator::GlobalGet { global_index }
                                    if matches!(
                                        library.globals.get(global_index as usize),
                                        Some(Global::MemoryBase)
                                    ) =>
                                {
                                    stack.push(0)
                                }
                                Operator::I32Const { value } if value >= 0 => {
                                    stack.push(value as u32)
                                }
                                Operator::I32Add => {
                                    let right = stack.pop().context("data offset operand")?;
                                    let left = stack.pop().context("data offset operand")?;
                                    stack.push(
                                        left.checked_add(right).context("data offset overflow")?,
                                    );
                                }
                                Operator::End => (),
                                _ => bail!(
                                    "internal helper data must be relative to its memory base"
                                ),
                            }
                        }
                        ensure!(stack.len() == 1, "invalid internal helper data offset");
                        library.data.push((stack[0], segment.data));
                    }
                }
                Payload::CustomSection(section) if section.name() == "dylink.0" => {
                    for subsection in wasmparser::Dylink0SectionReader::new(
                        wasmparser::BinaryReader::new(section.data(), section.data_offset()),
                    ) {
                        match subsection? {
                            Dylink0Subsection::MemInfo(info) => {
                                library.table_size = info.table_size;
                                library.table_alignment = 1u32
                                    .checked_shl(info.table_alignment)
                                    .context("helper table alignment overflow")?;
                                library.data_size = info.memory_size;
                                library.data_alignment = 1u32
                                    .checked_shl(info.memory_alignment)
                                    .context("helper data alignment overflow")?;
                            }
                            Dylink0Subsection::Needed(_) => {
                                bail!("internal helpers cannot load dynamic dependencies")
                            }
                            _ => (),
                        }
                    }
                }
                Payload::TableSection(section) => {
                    ensure!(section.count() == 0, "internal helpers do not own tables")
                }
                Payload::GlobalSection(section) => {
                    for global in section {
                        let global = global?;
                        ensure!(
                            !global.ty.mutable
                                && global.ty.content_type == wasmparser::ValType::I32,
                            "internal helpers cannot own mutable global state"
                        );
                        let mut initializer = global.init_expr.get_operators_reader();
                        let Operator::I32Const { value } = initializer.read()? else {
                            bail!("internal helper globals must be literal i32 constants");
                        };
                        ensure!(
                            matches!(initializer.read()?, Operator::End) && initializer.eof(),
                            "invalid internal helper constant initializer"
                        );
                        library.globals.push(Global::Constant(value));
                    }
                }
                Payload::MemorySection(section) => {
                    ensure!(
                        section.count() == 0,
                        "internal helpers borrow application memory"
                    )
                }
                Payload::ElementSection(elements) => {
                    for element in elements {
                        let element = element?;
                        let ElementKind::Active {
                            table_index: None | Some(0),
                            offset_expr,
                        } = element.kind
                        else {
                            bail!(
                                "internal helpers require active elements in their borrowed table"
                            );
                        };
                        let mut reader = offset_expr.get_operators_reader();
                        let offset = match reader.read()? {
                            Operator::GlobalGet { global_index }
                                if matches!(
                                    library.globals.get(global_index as usize),
                                    Some(Global::TableBase)
                                ) =>
                            {
                                0
                            }
                            _ => bail!("helper elements must start at their table base"),
                        };
                        ensure!(
                            matches!(reader.read()?, Operator::End) && reader.eof(),
                            "invalid helper table offset"
                        );
                        let ElementItems::Functions(functions) = element.items else {
                            bail!("internal helper elements must be function indices");
                        };
                        for (index, function) in functions.into_iter().enumerate() {
                            library.elements.push((offset + index as u32, function?));
                        }
                    }
                }
                Payload::StartSection { func, .. } => library.start = Some(func),
                Payload::TagSection(_) => {
                    bail!("unsupported stateful internal helper section")
                }
                _ => (),
            }
        }
        ensure!(
            library.bodies.len() == library.function_types.len(),
            "internal helper function declarations must match bodies"
        );
        Ok(library)
    }

    pub(crate) fn reachable(&self, entries: &[&str]) -> Result<BTreeSet<u32>> {
        let mut pending = entries
            .iter()
            .map(|entry| {
                self.exports
                    .get(*entry)
                    .copied()
                    .with_context(|| format!("missing internal helper export {entry}"))
            })
            .collect::<Result<Vec<_>>>()?;
        pending.extend(self.initializers());
        let mut reachable = BTreeSet::new();
        while let Some(function) = pending.pop() {
            if !reachable.insert(function) {
                continue;
            }
            if function < self.imports.len() as u32 {
                continue;
            }
            let body = self
                .bodies
                .get(function as usize - self.imports.len())
                .context("internal helper function reference out of range")?;
            for operator in body.get_operators_reader()? {
                match operator? {
                    Operator::Call { function_index }
                    | Operator::ReturnCall { function_index }
                    | Operator::RefFunc { function_index } => {
                        pending.push(function_index);
                    }
                    Operator::CallIndirect {
                        type_index,
                        table_index: 0,
                    }
                    | Operator::ReturnCallIndirect {
                        type_index,
                        table_index: 0,
                    } => {
                        for &(_, target) in &self.elements {
                            let ty = if let Some(&(_, ty)) = self.imports.get(target as usize) {
                                ty
                            } else {
                                self.function_types[target as usize - self.imports.len()]
                            };
                            if self.types[ty as usize] == self.types[type_index as usize] {
                                pending.push(target);
                            }
                        }
                    }
                    _ => (),
                }
            }
        }
        Ok(reachable)
    }

    /// Relocation and constructor entry points must survive helper reachability pruning.
    pub(crate) fn initializers(&self) -> impl Iterator<Item = u32> + '_ {
        [
            self.start,
            self.exports.get("__wasm_apply_data_relocs").copied(),
            self.exports.get("__wasm_call_ctors").copied(),
            self.exports.get("_initialize").copied(),
        ]
        .into_iter()
        .flatten()
    }
}

pub(crate) struct Relocations<'a> {
    pub(crate) functions: &'a BTreeMap<u32, u32>,
    pub(crate) globals: &'a [Global],
    pub(crate) type_base: u32,
    pub(crate) memory_base: u32,
    pub(crate) stack_global: u32,
    pub(crate) table_base: u32,
}

impl Reencode for Relocations<'_> {
    type Error = String;
    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<String>> {
        self.functions.get(&index).copied().ok_or_else(|| {
            reencode::Error::UserError(format!("unselected helper function {index}"))
        })
    }
    fn type_index(&mut self, index: u32) -> Result<u32, reencode::Error<String>> {
        Ok(self.type_base + index)
    }
    fn table_index(&mut self, index: u32) -> Result<u32, reencode::Error<String>> {
        if index == 0 {
            Ok(0)
        } else {
            Err(reencode::Error::UserError(
                "unexpected helper table index".into(),
            ))
        }
    }
    fn global_index(&mut self, index: u32) -> Result<u32, reencode::Error<String>> {
        if matches!(self.globals.get(index as usize), Some(Global::Stack)) {
            Ok(self.stack_global)
        } else {
            Err(reencode::Error::UserError(
                "helper global is not mutable application state".into(),
            ))
        }
    }
    fn instruction<'a>(
        &mut self,
        operator: Operator<'a>,
    ) -> Result<Instruction<'a>, reencode::Error<String>> {
        if let Operator::GlobalGet { global_index } = operator {
            match self.globals.get(global_index as usize) {
                Some(Global::MemoryBase) => {
                    return Ok(Instruction::I32Const(self.memory_base as i32));
                }
                Some(Global::TableBase) => {
                    return Ok(Instruction::I32Const(self.table_base as i32));
                }
                Some(Global::Constant(value)) => return Ok(Instruction::I32Const(*value)),
                _ => (),
            }
        }
        reencode::utils::instruction(self, operator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_library_parse() {
        let lib = Library::parse(SEARCH).expect("parse search helper");
        assert!(lib.exports.contains_key("str_find_substring"));
        assert!(lib.exports.contains_key("str_scalar_to_byte"));
        let reachable = lib
            .reachable(&["str_find_substring"])
            .expect("reachability check");
        assert!(!reachable.is_empty());
    }

    #[test]
    fn test_text_library_parse() {
        let lib = Library::parse(TEXT).expect("parse text helper");
        assert!(lib.exports.contains_key("str_code_point_at"));
        assert!(lib.exports.contains_key("str_from_code_point"));
        assert!(lib.exports.contains_key("str_case_convert"));
        assert!(lib.exports.contains_key("str_split_count"));
        assert!(lib.exports.contains_key("str_split_populate"));
        assert!(lib.exports.contains_key("str_join_total_len"));
        assert!(lib.exports.contains_key("str_join"));
        let reachable = lib
            .reachable(&["str_code_point_at", "str_from_code_point"])
            .expect("reachability check");
        assert!(!reachable.is_empty());
    }
}
