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

pub(crate) const FETCH: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fetch.wasm"));
pub(crate) const SEARCH: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/search.wasm"));
pub(crate) const TEXT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/text.wasm"));
pub(crate) const TIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/time.wasm"));
pub(crate) const JSON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/json.wasm"));
pub(crate) const NUMBER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/number.wasm"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LibraryId {
    Fetch,
    Search,
    Text,
    Json,
    Time,
    Number,
}

impl LibraryId {
    pub(crate) fn for_entry(entry: &str) -> Result<Self> {
        Ok(match entry {
            "number_remainder" => Self::Number,
            "fetch_status_text" | "fetch_url" | "fetch_redirect" | "fetch_decode"
            | "fetch_method" | "fetch_header_value" | "fetch_header_size" | "fetch_header_get"
            | "fetch_header_name" | "fetch_header_edit" => Self::Fetch,
            "str_find_substring" | "str_scalar_to_byte" => Self::Search,
            "str_code_point_at"
            | "str_from_code_point"
            | "str_case_convert"
            | "str_split_count"
            | "str_split_populate"
            | "str_join_total_len"
            | "str_join" => Self::Text,
            "json_measure" | "json_populate" | "json_serialized_size" | "json_serialize" => {
                Self::Json
            }
            "time_date_iso"
            | "time_date_parse"
            | "time_date_part"
            | "time_instant_parse"
            | "time_utc_parse"
            | "time_instant_from_ms"
            | "time_instant_ms"
            | "time_instant_format"
            | "time_plain_parse"
            | "time_plain_date_parse"
            | "time_plain_add_days"
            | "time_plain_part"
            | "time_plain_format"
            | "time_plain_date_format" => Self::Time,
            _ => bail!("unknown helper entry {entry}"),
        })
    }

    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Fetch => FETCH,
            Self::Search => SEARCH,
            Self::Text => TEXT,
            Self::Json => JSON,
            Self::Time => TIME,
            Self::Number => NUMBER,
        }
    }
}

/// One placement policy for both the guest allocator and linked Rust helpers.
pub(crate) struct HelperMemory {
    end: u32,
    pub(crate) needs_stack: bool,
    stack_bytes: u32,
}

impl HelperMemory {
    pub(crate) fn new(data_end: u32) -> Self {
        Self {
            end: data_end.max(1024),
            needs_stack: false,
            stack_bytes: 0,
        }
    }

    pub(crate) fn place(&mut self, library: &Library) -> Result<u32> {
        let base = align_to(self.end, library.data_alignment)?;
        self.end = base
            .checked_add(library.data_size)
            .context("helper data exceeds memory32")?;
        self.needs_stack |= library.globals.contains(&Global::Stack);
        if library.globals.contains(&Global::Stack) {
            self.stack_bytes = self
                .stack_bytes
                .max(library.fixed_stack_bound().unwrap_or(65_536));
        }
        Ok(base)
    }

    pub(crate) fn stack_top(&self) -> Result<u32> {
        align_to(self.end, 16)?
            .checked_add(self.stack_bytes)
            .context("helper stack exceeds memory32")
    }
}

pub(crate) fn align_to(value: u32, alignment: u32) -> Result<u32> {
    ensure!(
        alignment.is_power_of_two(),
        "helper alignment must be a power of two"
    );
    Ok(value
        .checked_add(alignment - 1)
        .context("helper alignment exceeds memory32")?
        & !(alignment - 1))
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
    /// Acyclic helpers with fixed LLVM stack frames need at most the sum of all
    /// frames. Unrecognized stack writes, recursion, and indirect calls retain
    /// the conservative reservation. The allocator and linker use this same bound.
    fn fixed_stack_bound(&self) -> Option<u32> {
        use wasmparser::Operator as Op;
        if !self.imports.is_empty() {
            return None;
        }
        let stack = self
            .globals
            .iter()
            .position(|global| *global == Global::Stack)? as u32;
        let mut graph = Vec::new();
        let mut total = 0_u32;
        for body in &self.bodies {
            let ops = body
                .get_operators_reader()
                .ok()?
                .into_iter()
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            let mut frame = None;
            let mut depth = 0;
            let mut restored = false;
            let mut calls = Vec::new();
            for (index, op) in ops.iter().enumerate() {
                match op {
                    Op::Call { function_index } => calls.push(*function_index as usize),
                    Op::CallIndirect { .. }
                    | Op::CallRef { .. }
                    | Op::ReturnCall { .. }
                    | Op::ReturnCallIndirect { .. }
                    | Op::ReturnCallRef { .. } => return None,
                    Op::GlobalSet { global_index } if *global_index == stack => {
                        if index == 4
                            && let [
                                Op::GlobalGet { global_index },
                                Op::I32Const { value },
                                Op::I32Sub,
                                Op::LocalTee { local_index },
                            ] = &ops[..index]
                            && *global_index == stack
                            && *value > 0
                            && frame.is_none()
                        {
                            frame = Some((*local_index, *value));
                            total = total.checked_add(*value as u32)?;
                        } else if index >= 3
                            && let [
                                Op::LocalGet { local_index },
                                Op::I32Const { value },
                                Op::I32Add,
                            ] = &ops[index - 3..index]
                            && frame == Some((*local_index, *value))
                            && depth == 0
                            && !restored
                        {
                            restored = true;
                        } else {
                            return None;
                        }
                    }
                    Op::LocalSet { local_index } | Op::LocalTee { local_index }
                        if frame.is_some_and(|(slot, _)| slot == *local_index) =>
                    {
                        return None;
                    }
                    Op::Block { .. } | Op::Loop { .. } | Op::If { .. } => depth += 1,
                    Op::End if depth > 0 => depth -= 1,
                    Op::Return | Op::End if frame.is_some() && !restored => return None,
                    Op::Br { relative_depth } | Op::BrIf { relative_depth }
                        if frame.is_some() && *relative_depth >= depth =>
                    {
                        return None;
                    }
                    Op::BrTable { targets }
                        if frame.is_some()
                            && (targets.default() >= depth
                                || targets.targets().any(|target| {
                                    target.map_or(true, |target| target >= depth)
                                })) =>
                    {
                        return None;
                    }
                    _ => {}
                }
            }
            graph.push(calls);
        }
        fn visit(index: usize, graph: &[Vec<usize>], marks: &mut [u8]) -> Option<()> {
            match *marks.get(index)? {
                1 => return None,
                2 => return Some(()),
                _ => {}
            }
            marks[index] = 1;
            for &callee in &graph[index] {
                visit(callee, graph, marks)?;
            }
            marks[index] = 2;
            Some(())
        }
        let mut marks = vec![0; graph.len()];
        for index in 0..graph.len() {
            visit(index, &graph, &mut marks)?;
        }
        Some(total.max(1024))
    }

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

    pub(crate) fn reachable<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeSet<u32>> {
        let mut pending = entries
            .into_iter()
            .map(|entry| {
                self.exports
                    .get(entry)
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
    fn time_helper_has_a_checked_small_stack() {
        let library = Library::parse(TIME).unwrap();
        assert!(
            library.fixed_stack_bound().is_some_and(|size| size < 8192),
            "{:?}",
            library.fixed_stack_bound()
        );
    }

    #[test]
    fn unbounded_or_unbalanced_helpers_keep_the_full_stack_reservation() {
        for body in [
            "(func call 0)",
            "(func (local i32) loop global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 br 0 end)",
            "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 return)",
            "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 i32.const 1 if return end local.get 0 i32.const 16 i32.add global.set 0)",
        ] {
            let bytes = wat::parse_str(format!(
                "(module (import \"env\" \"__stack_pointer\" (global (mut i32))) {body})"
            ))
            .unwrap();
            let library = Library::parse(Box::leak(bytes.into_boxed_slice())).unwrap();
            assert_eq!(library.fixed_stack_bound(), None, "{body}");
        }
    }

    #[test]
    fn test_search_library_parse() {
        let lib = Library::parse(SEARCH).expect("parse search helper");
        assert!(lib.exports.contains_key("str_find_substring"));
        assert!(lib.exports.contains_key("str_scalar_to_byte"));
        let reachable = lib
            .reachable(["str_find_substring"])
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
            .reachable(["str_code_point_at", "str_from_code_point"])
            .expect("reachability check");
        assert!(!reachable.is_empty());
    }
}
