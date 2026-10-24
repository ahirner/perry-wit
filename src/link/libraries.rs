//! Guest helper libraries: parsing, reachability analysis, and relocations for prebuilt
//! `wasm32-unknown-unknown` helper artifacts.

use std::collections::{BTreeMap, BTreeSet};

use super::parsed::ParsedModule;
use anyhow::{Context, Result, bail, ensure};
use pliron::graph::{ControlFlowGraph, HasLabel, traversals::region::sccs_in_topological_order};
use wasm_encoder::Instruction;
use wasm_encoder::reencode::{self, Reencode};
use wasmparser::{Dylink0Subsection, ElementItems, ElementKind, ExternalKind, Operator, TypeRef};

pub(crate) const FETCH: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/fetch.wasm"));
pub(crate) const TEXT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/text.wasm"));
pub(crate) const TIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/time.wasm"));
const JSON_GRAPH: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/json-graph.wasm"));
const JSON_CURSOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/json-cursor.wasm"));
const JSON_COPY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/json-copy.wasm"));
pub(crate) const JSON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/json.wasm"));
const NORMALIZATION: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/runtime/normalization.wasm"));
const DECIMAL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/decimal.wasm"));
pub(crate) const NUMBER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/number.wasm"));
pub(crate) const COLLECTIBLE_RUNTIME: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/runtime/runtime-collectible.wasm"
));
pub(crate) const RUNTIME: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/runtime.wasm"));
pub(crate) const STD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/std.wasm"));
pub(crate) const FS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/fs.wasm"));
pub(crate) const WEB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/web.wasm"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LibraryId {
    Runtime,
    Std,
    Fs,
    Web,
    Fetch,
    Text,
    Normalization,
    Json,
    JsonGraph,
    JsonCursor,
    JsonCopy,
    Time,
    Number,
    Decimal,
}

impl LibraryId {
    pub(crate) fn for_entry(entry: &str) -> Result<Self> {
        Ok(match entry {
            entry if entry.starts_with("perry_") || entry == "cabi_realloc" => Self::Runtime,
            "std_parse_float" | "std_string_to_number" => Self::Decimal,
            "number_format" => Self::Number,
            entry if entry.starts_with("std_") => Self::Std,
            entry if entry.starts_with("fs_") => Self::Fs,
            entry if entry.starts_with("web_") => Self::Web,
            "fetch_status_text" | "fetch_url" | "fetch_url_value" | "fetch_method"
            | "fetch_header_value" | "fetch_header_name" => Self::Fetch,
            "str_case_convert" => Self::Text,
            "str_normalize" => Self::Normalization,
            "json_descriptor_decode_graph" | "json_descriptor_stringify" => Self::JsonGraph,
            "json_descriptor_decode_cursor" => Self::JsonCursor,
            "json_descriptor_copy" => Self::JsonCopy,
            entry if entry.starts_with("json_") => Self::Json,
            // The time helper exports every `time_` entry, so new ones cannot drift out of a list.
            entry if entry.starts_with("time_") => Self::Time,
            _ => bail!("unknown helper entry {entry}"),
        })
    }

    pub(crate) fn for_memory(self, collectible: bool) -> &'static [u8] {
        if self == Self::Runtime && collectible {
            COLLECTIBLE_RUNTIME
        } else {
            self.bytes()
        }
    }

    pub(crate) fn bytes(self) -> &'static [u8] {
        match self {
            Self::Runtime => RUNTIME,
            Self::Std => STD,
            Self::Fs => FS,
            Self::Web => WEB,
            Self::Fetch => FETCH,
            Self::Text => TEXT,
            Self::Normalization => NORMALIZATION,
            Self::Json => JSON,
            Self::JsonGraph => JSON_GRAPH,
            Self::JsonCursor => JSON_CURSOR,
            Self::JsonCopy => JSON_COPY,
            Self::Time => TIME,
            Self::Number => NUMBER,
            Self::Decimal => DECIMAL,
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

    /// Place `library`, whose `reachable` functions (`None`: all) are linked.
    pub(crate) fn place(
        &mut self,
        library: &Library<'_>,
        reachable: Option<&BTreeSet<u32>>,
    ) -> Result<u32> {
        let base = align_to(self.end, library.data_alignment)?;
        self.end = base
            .checked_add(library.data_size)
            .context("helper data exceeds memory32")?;
        self.needs_stack |= library.globals.contains(&Global::Stack);
        if library.globals.contains(&Global::Stack) {
            // Guest libraries share the shadow stack and call one another. Each library's
            // bound treats imports as leaves, so their reservations must compose by addition.
            self.stack_bytes = self
                .stack_bytes
                .checked_add(library.fixed_stack_bound(reachable).unwrap_or(65_536))
                .context("combined helper stack exceeds memory32")?;
        }
        Ok(base)
    }

    /// The end of the placed static data, where the helper stack's reservation starts.
    #[allow(dead_code)]
    pub(crate) fn data_end(&self) -> u32 {
        self.end
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
    /// `GOT.mem.__heap_base`: the end of linked data and stack, where the heap starts.
    HeapBase,
    Constant(i32),
}

pub(crate) struct Library<'a> {
    pub(super) module: ParsedModule<'a>,
    pub(crate) exports: BTreeMap<&'a str, u32>,
    pub(crate) globals: Vec<Global>,
    pub(crate) data: Vec<(u32, &'a [u8])>,
    pub(crate) data_size: u32,
    pub(crate) data_alignment: u32,
    pub(crate) elements: Vec<(u32, u32)>,
    pub(crate) table_size: u32,
    pub(crate) table_alignment: u32,
}

struct HelperCalls<'a> {
    successors: &'a [Vec<usize>],
    predecessors: Vec<Vec<usize>>,
}

impl<'a> HelperCalls<'a> {
    fn new(successors: &'a [Vec<usize>]) -> Self {
        let mut predecessors = vec![Vec::new(); successors.len()];
        for (caller, calls) in successors.iter().enumerate() {
            for &callee in calls {
                predecessors[callee].push(caller);
            }
        }
        Self {
            successors,
            predecessors,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FunctionIndex(usize);

impl HasLabel<()> for FunctionIndex {
    fn label(&self, _: &()) -> String {
        self.0.to_string()
    }
}

impl ControlFlowGraph<()> for HelperCalls<'_> {
    type Node = FunctionIndex;
    fn num_successors(&self, _: &(), node: &FunctionIndex) -> usize {
        self.successors[node.0].len()
    }
    fn get_successor(&self, _: &(), node: &FunctionIndex, index: usize) -> FunctionIndex {
        FunctionIndex(self.successors[node.0][index])
    }
    fn num_predecessors(&self, _: &(), node: &FunctionIndex) -> usize {
        self.predecessors[node.0].len()
    }
    fn get_predecessor(&self, _: &(), node: &FunctionIndex, index: usize) -> FunctionIndex {
        FunctionIndex(self.predecessors[node.0][index])
    }
    fn entry_node(&self, _: &()) -> Option<FunctionIndex> {
        (!self.successors.is_empty()).then_some(FunctionIndex(0))
    }
    fn nodes<'a>(&'a self, _: &'a ()) -> Box<dyn Iterator<Item = FunctionIndex> + 'a> {
        Box::new((0..self.successors.len()).map(FunctionIndex))
    }
}

impl<'a> Library<'a> {
    /// Bound the deepest linear-memory shadow-stack frame chain among reachable helpers.
    /// Recursive components that allocate no frame contribute only their callees' depth.
    /// Growing recursion, unrecognized stack writes, and indirect calls retain the
    /// conservative reservation.
    fn fixed_stack_bound(&self, reachable: Option<&BTreeSet<u32>>) -> Option<u32> {
        use wasmparser::Operator as Op;
        macro_rules! return_value_ops {
            ($(@$proposal:ident $operator:ident $({ $($field:ident: $ty:ty),* })? => $visitor:ident ($($annotation:tt)*))*) => {
                |operator: &Op<'_>| match operator {
                    $(Op::$operator $({ $($field: _),* })? => return_value_ops!(@operator $operator $($annotation)*),)*
                    _ => false,
                }
            };
            (@operator Unreachable $($annotation:tt)*) => { false };
            (@operator GlobalSet $($annotation:tt)*) => { false };
            (@operator $operator:ident arity custom) => { false };
            (@operator $operator:ident $($annotation:tt)*) => { true };
        }
        // Fixed-arity operators cannot branch or call. Global writes remain excluded.
        let computes_return_value = wasmparser::for_each_operator!(return_value_ops);
        // Imported functions are leaves here; HelperMemory adds the guest-library bounds.
        let imported = self.module.imports.len();
        let stack = self
            .globals
            .iter()
            .position(|global| *global == Global::Stack)? as u32;
        let mut graph = Vec::new();
        let mut frames = Vec::new();
        for (defined, body) in self.module.bodies.iter().enumerate() {
            if reachable.is_some_and(|set| !set.contains(&((imported + defined) as u32))) {
                graph.push(Vec::new());
                frames.push(0);
                continue;
            }
            let ops = body
                .get_operators_reader()
                .ok()?
                .into_iter()
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            let mut frame = None;
            let mut depth = 0;
            let mut restored = false;
            let mut restored_returns = BTreeSet::new();
            let mut calls = Vec::new();
            // Structured reachability: a function end that no path reaches (a loop left only
            // through `return`) cannot leave the frame in place.
            let mut controls: Vec<Control> = Vec::new();
            let mut reachable = true;
            for (index, op) in ops.iter().enumerate() {
                match op {
                    Op::Call { function_index } if (*function_index as usize) < imported => {}
                    Op::Call { function_index } => {
                        let callee = *function_index as usize - imported;
                        if callee >= self.module.bodies.len() {
                            return None;
                        }
                        calls.push(callee);
                    }
                    Op::CallIndirect { .. }
                    | Op::CallRef { .. }
                    | Op::ReturnCall { .. }
                    | Op::ReturnCallIndirect { .. }
                    | Op::ReturnCallRef { .. } => {
                        return None;
                    }
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
                        } else if index >= 3
                            && let [
                                Op::LocalGet { local_index },
                                Op::I32Const { value },
                                Op::I32Add,
                            ] = &ops[index - 3..index]
                            && frame == Some((*local_index, *value))
                            && !restored
                        {
                            if depth == 0 {
                                restored = true;
                            } else {
                                let tail = &ops[index + 1..];
                                let returned =
                                    tail.iter().position(|op| !computes_return_value(op))?;
                                if !matches!(tail[returned], Op::Return) {
                                    return None;
                                }
                                restored_returns.insert(index + 1 + returned);
                            }
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
                    Op::Return
                        if frame.is_some() && !restored && !restored_returns.contains(&index) =>
                    {
                        return None;
                    }
                    Op::End
                        if frame.is_some()
                            && !restored
                            && reachable
                            && !matches!(ops.get(index.wrapping_sub(1)), Some(Op::Unreachable)) =>
                    {
                        return None;
                    }
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
                reachable = Control::step(&mut controls, op, reachable);
            }
            let frame_bytes = if let Some((_, size)) = frame {
                size as u32
            } else if ops
                .iter()
                .any(|op| matches!(op, Op::GlobalGet { global_index } if *global_index == stack))
            {
                // LLVM leaf functions may reserve scratch below the stack pointer without
                // publishing it. No call can use that unpublished space concurrently.
                let [
                    Op::GlobalGet { global_index },
                    Op::I32Const { value },
                    Op::I32Sub,
                    Op::LocalSet { .. } | Op::LocalTee { .. },
                ] = ops.get(..4)?
                else {
                    return None;
                };
                if *global_index != stack || *value <= 0 || ops[4..].iter().any(|op| {
                    matches!(op, Op::Call { .. })
                        || matches!(op, Op::GlobalGet { global_index } if *global_index == stack)
                }) {
                    return None;
                }
                *value as u32
            } else {
                0
            };
            graph.push(calls);
            frames.push(frame_bytes);
        }
        let graph = HelperCalls::new(&graph);
        let mut depths = vec![0_u32; frames.len()];
        for component in sccs_in_topological_order(&(), &graph).into_iter().rev() {
            if component.is_cyclic && component.nodes.iter().any(|node| frames[node.0] != 0) {
                return None;
            }
            let mut depth = 0_u32;
            for node in &component.nodes {
                let callee_depth = graph.successors[node.0]
                    .iter()
                    .try_fold(0_u32, |deepest, &callee| {
                        Some(deepest.max(*depths.get(callee)?))
                    })?;
                depth = depth.max(frames[node.0].checked_add(callee_depth)?);
            }
            for node in component.nodes {
                depths[node.0] = depth;
            }
        }
        let deepest = depths.into_iter().max().unwrap_or(0);
        Some(deepest)
    }

    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut library = Self {
            module: ParsedModule::parse(bytes)?,
            exports: BTreeMap::new(),
            globals: vec![],
            data: vec![],
            data_size: 0,
            data_alignment: 1,
            elements: vec![],
            table_size: 0,
            table_alignment: 1,
        };
        ensure!(
            library
                .module
                .tables
                .as_ref()
                .is_none_or(|s| s.count() == 0),
            "internal helpers do not own tables"
        );
        ensure!(
            library
                .module
                .memories
                .as_ref()
                .is_none_or(|s| s.count() == 0),
            "internal helpers borrow application memory"
        );
        ensure!(
            library.module.tags.is_none(),
            "unsupported stateful internal helper section"
        );
        for &(module, name, _) in &library.module.imports {
            ensure!(module != "env", "unsupported helper import {module}.{name}");
        }
        for import in library.module.other_imports.drain(..) {
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
                ("GOT.mem", "__heap_base", TypeRef::Global(_)) => {
                    library.globals.push(Global::HeapBase);
                }
                _ => bail!(
                    "unsupported helper import {}.{}",
                    import.module,
                    import.name
                ),
            }
        }
        if let Some(section) = library.module.globals.take() {
            for global in section {
                let global = global?;
                ensure!(
                    !global.ty.mutable && global.ty.content_type == wasmparser::ValType::I32,
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
        if let Some(exports) = library.module.exports.take() {
            for export in exports {
                let export = export?;
                if export.kind == ExternalKind::Func {
                    library.exports.insert(export.name, export.index);
                }
            }
        }
        if let Some(data) = library.module.data.take() {
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
                        Operator::I32Const { value } if value >= 0 => stack.push(value as u32),
                        Operator::I32Add => {
                            let right = stack.pop().context("data offset operand")?;
                            let left = stack.pop().context("data offset operand")?;
                            stack.push(left.checked_add(right).context("data offset overflow")?);
                        }
                        Operator::End => (),
                        _ => bail!("internal helper data must be relative to its memory base"),
                    }
                }
                ensure!(stack.len() == 1, "invalid internal helper data offset");
                library.data.push((stack[0], segment.data));
            }
        }
        if let Some(elements) = library.module.elements.take() {
            for element in elements {
                let element = element?;
                let ElementKind::Active {
                    table_index: None | Some(0),
                    offset_expr,
                } = element.kind
                else {
                    bail!("internal helpers require active elements in their borrowed table");
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
        for section in &library.module.custom {
            if section.name() == "dylink.0" {
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
        }
        Ok(library)
    }

    pub(crate) fn reachable<'entry>(
        &self,
        entries: impl IntoIterator<Item = &'entry str>,
        folded_initializers: &BTreeSet<u32>,
        selected: &BTreeSet<u32>,
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
        pending.extend(
            self.initializers()
                .filter(|f| !folded_initializers.contains(f)),
        );
        let mut reachable = BTreeSet::new();
        while let Some(function) = pending.pop() {
            if selected.contains(&function) || !reachable.insert(function) {
                continue;
            }
            if function < self.module.imports.len() as u32 {
                continue;
            }
            let body = self
                .module
                .bodies
                .get(function as usize - self.module.imports.len())
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
                            let ty = if let Some(&(_, _, ty)) =
                                self.module.imports.get(target as usize)
                            {
                                ty
                            } else {
                                self.module.function_types
                                    [target as usize - self.module.imports.len()]
                            };
                            if self.module.types[ty as usize]
                                == self.module.types[type_index as usize]
                            {
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
            self.module.start,
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
    pub(crate) heap_base: u32,
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
                Some(Global::HeapBase) => {
                    return Ok(Instruction::I32Const(self.heap_base as i32));
                }
                _ => (),
            }
        }
        reencode::utils::instruction(self, operator)
    }
}

/// A block, loop or `if` for [Library::fixed_stack_bound]'s reachability: whether its end
/// is reached by falling through, by a branch to it, or (an `if` without `else`) by the
/// condition being false.
struct Control {
    looping: bool,
    conditional: bool,
    otherwise: bool,
    entry: bool,
    branched: bool,
}

impl Control {
    /// Whether the code after `op` is reachable, given that `op` is (`reachable`).
    fn step(controls: &mut Vec<Control>, op: &wasmparser::Operator<'_>, reachable: bool) -> bool {
        use wasmparser::Operator as Op;
        let branch = |controls: &mut Vec<Control>, depth: u32| {
            let len = controls.len();
            if let Some(target) = len
                .checked_sub(1 + depth as usize)
                .map(|at| &mut controls[at])
                && !target.looping
            {
                target.branched |= reachable;
            }
        };
        match op {
            Op::Block { .. } | Op::Loop { .. } | Op::If { .. } => {
                controls.push(Control {
                    looping: matches!(op, Op::Loop { .. }),
                    conditional: matches!(op, Op::If { .. }),
                    otherwise: false,
                    entry: reachable,
                    branched: false,
                });
                reachable
            }
            Op::Else => match controls.last_mut() {
                Some(control) => {
                    control.branched |= reachable;
                    control.otherwise = true;
                    control.entry
                }
                None => reachable,
            },
            Op::End => match controls.pop() {
                Some(control) if control.looping => reachable,
                Some(control) => {
                    reachable
                        || control.branched
                        || (control.conditional && !control.otherwise && control.entry)
                }
                None => reachable,
            },
            Op::Br { relative_depth } => {
                branch(controls, *relative_depth);
                false
            }
            Op::BrIf { relative_depth } => {
                branch(controls, *relative_depth);
                reachable
            }
            Op::BrTable { targets } => {
                branch(controls, targets.default());
                for target in targets.targets().flatten() {
                    branch(controls, target);
                }
                false
            }
            Op::Return | Op::Unreachable => false,
            _ => reachable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_libraries_have_checked_small_stacks() {
        for (name, bytes) in [
            ("time", TIME),
            ("number", NUMBER),
            ("runtime", RUNTIME),
            ("runtime_collectible", COLLECTIBLE_RUNTIME),
            ("fs", FS),
            ("web", WEB),
            ("json", JSON),
            ("std", STD),
            ("text", TEXT),
        ] {
            let library = Library::parse(bytes).unwrap();
            assert!(
                library
                    .fixed_stack_bound(None)
                    .is_some_and(|size| size < 8192),
                "{name}: {:?}",
                library.fixed_stack_bound(None)
            );
        }
    }

    fn bound_for(body: &str) -> Option<u32> {
        let bytes = wat::parse_str(format!(
            "(module (import \"env\" \"__stack_pointer\" (global (mut i32))) {body})"
        ))
        .unwrap();
        wasmparser::Validator::new().validate_all(&bytes).unwrap();
        Library::parse(&bytes).unwrap().fixed_stack_bound(None)
    }

    #[test]
    fn unbounded_or_unbalanced_helpers_keep_the_full_stack_reservation() {
        for body in [
            "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 i32.const 1 if local.get 0 i32.const 16 i32.add global.set 0 return end)",
            "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 call 0 local.get 0 i32.const 16 i32.add global.set 0)",
            "(func (local i32) loop global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 br 0 end)",
            "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 return)",
            "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 i32.const 1 if return end local.get 0 i32.const 16 i32.add global.set 0)",
            // A branch out of the block reaches the end with the frame in place.
            "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 block i32.const 1 br_if 0 local.get 0 i32.const 16 i32.add global.set 0 return end)",
        ] {
            assert_eq!(bound_for(body), None, "{body}");
        }
    }

    #[test]
    fn zero_frame_recursion_preserves_callee_and_caller_depths() {
        for (body, bound) in [
            ("(func call 0)", 0),
            ("(func call 1) (func call 0)", 0),
            (
                "(func call 1) (func call 0 call 2) (func (local i32) global.get 0 i32.const 4096 i32.sub local.tee 0 global.set 0 local.get 0 i32.const 4096 i32.add global.set 0)",
                4096,
            ),
            (
                "(func (local i32) global.get 0 i32.const 2048 i32.sub local.tee 0 global.set 0 call 1 local.get 0 i32.const 2048 i32.add global.set 0) (func call 2) (func call 1 call 3) (func (local i32) global.get 0 i32.const 4096 i32.sub local.tee 0 global.set 0 local.get 0 i32.const 4096 i32.add global.set 0)",
                6144,
            ),
            (
                "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 loop i32.const 1 if local.get 0 i32.const 16 i32.add global.set 0 return end br 0 end)",
                16,
            ),
        ] {
            assert_eq!(bound_for(body), Some(bound), "{body}");
        }
    }

    #[test]
    fn return_values_can_be_computed_after_conditional_stack_restoration() {
        for (parameter, computation) in [
            ("f64", "local.get 0 f64.neg local.get 0 local.get 1 select"),
            (
                "i32",
                "local.get 0 i32.const 8 i32.mul i32.const 255 i32.and",
            ),
            (
                "i64",
                "local.get 0 i64.const 56 i64.shl i64.const 56 i64.shr_s",
            ),
        ] {
            let body = format!(
                "(func (param {parameter} i32) (result {parameter}) (local i32)
                  global.get 0 i32.const 16 i32.sub local.tee 2 global.set 0
                  local.get 1 if local.get 2 i32.const 16 i32.add global.set 0 {computation} return end
                  local.get 2 i32.const 16 i32.add global.set 0 local.get 0)"
            );
            assert_eq!(bound_for(&body), Some(16), "{computation}");
        }
    }

    #[test]
    fn unpublished_leaf_scratch_contributes_to_caller_depth() {
        for (scratch, bound) in [
            (
                "global.get 0 i32.const 1536 i32.sub local.set 0",
                Some(1552),
            ),
            (
                "global.get 0 i32.const 1536 i32.sub local.tee 0 drop",
                Some(1552),
            ),
            (
                "global.get 0 i32.const 1536 i32.sub local.set 0 call 2",
                None,
            ),
            (
                "global.get 0 i32.const 1536 i32.sub local.set 0 global.get 0 drop",
                None,
            ),
            ("global.get 0 local.set 0", None),
        ] {
            let body = format!(
                "(func (local i32) global.get 0 i32.const 16 i32.sub local.tee 0 global.set 0 call 1 local.get 0 i32.const 16 i32.add global.set 0)
                 (func (local i32) {scratch}) (func)"
            );
            assert_eq!(bound_for(&body), bound, "{scratch}");
        }
    }
}
