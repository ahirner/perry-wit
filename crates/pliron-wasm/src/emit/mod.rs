//! `wasm` dialect → core Wasm binary.
//!
//! Control flow is structured with Ramsey's dominator-tree translation ("Beyond Relooper",
//! ICFP 2022): every reducible CFG becomes nested `block`/`loop`/`if` with no code duplication
//! and no helper variables. Loop headers get a `loop`, blocks with several forward
//! predecessors ("merge nodes") get an enclosing `block` that forward branches exit, and every
//! other block is emitted inline at its single forward predecessor.
//!
//! Values: a single-use result defined in the same block as its user is evaluated in place on
//! the operand stack (tree folding), and constants are rematerialized at each use. Effects and
//! traps keep their order; pure ops move freely.
//! Every other value, and every block argument, lives in a local. Block arguments are written
//! at each edge by pushing all incoming values before setting any parameter, so cyclic moves
//! (`^loop(b, a)`) need no temporaries.
//!
//! Submodules: [function] holds the per-function analyses, [schedule] the tree folding,
//! [locals] local allocation, [structure] the control-flow structuring, [values] instruction
//! selection and [peephole] the final rewrites.
mod function;
mod locals;
mod peephole;
mod schedule;
mod structure;
mod values;

use crate::{
    ops::{ImportFuncOp, export_names},
    types::val_type,
};
use function::FunctionEmitter;
use pliron::{
    builtin::{
        op_interfaces::{SingleBlockRegionInterface, SymbolOpInterface},
        ops::{FuncOp, ModuleOp},
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    context::Context,
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
};
use std::collections::HashMap;
use thiserror::Error;
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, FunctionSection,
    ImportSection, IndirectNameMap, MemorySection, MemoryType, Module, NameMap, NameSection,
    TypeSection, ValType,
};

#[derive(Debug, Error)]
pub enum EmitError {
    #[error("function {function}: control flow is irreducible at block {block}")]
    Irreducible { function: String, block: String },
    #[error("function {function}: {op} is not part of the wasm dialect")]
    UnsupportedOp { function: String, op: String },
    #[error("{0} has a type that is not a Wasm value type")]
    ValueType(String),
    #[error("function {function}: {message}")]
    Malformed { function: String, message: String },
}

type EmitResult<T> = Result<T, EmitError>;

/// Linear memory 0 of the emitted module.
#[derive(Clone, Debug, Default)]
pub enum Memory {
    /// No memory; loads and stores fail validation.
    #[default]
    None,
    /// Import memory 0, e.g. from a runtime module that owns the allocator.
    Import {
        module: String,
        name: String,
        min_pages: u64,
    },
    /// Define memory 0, optionally exporting it.
    Define {
        min_pages: u64,
        export: Option<String>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct EmitOptions {
    pub memory: Memory,
    /// Active data segments of memory 0.
    pub data: Vec<DataSegment>,
    /// Emit a `name` section with function and local names.
    pub names: bool,
}

/// Bytes placed at a fixed address of memory 0 at instantiation.
#[derive(Clone, Debug, Default)]
pub struct DataSegment {
    pub offset: u32,
    pub bytes: Vec<u8>,
}

/// Parameter and result types of a function type.
type Signature = (Vec<ValType>, Vec<ValType>);

/// The Wasm signature of the function type `ty` of `what`.
fn signature(ctx: &Context, ty: TypeHandle, what: &str) -> EmitResult<Signature> {
    let ty = TypedHandle::<FunctionType>::from_handle(ty, ctx)
        .map_err(|_| EmitError::ValueType(what.to_string()))?;
    let ty = ty.deref(ctx);
    let map = |types: Vec<TypeHandle>| -> EmitResult<Vec<ValType>> {
        types
            .into_iter()
            .map(|t| val_type(ctx, t).ok_or_else(|| EmitError::ValueType(what.to_string())))
            .collect()
    };
    Ok((map(ty.arg_types())?, map(ty.res_types())?))
}

/// Encode `module` as a core Wasm module. Imports get the lowest function indices, then
/// functions follow in module order.
pub fn emit_module(ctx: &Context, module: &ModuleOp, options: &EmitOptions) -> EmitResult<Vec<u8>> {
    let block = module.get_body(ctx, 0);
    let mut imports = Vec::new();
    let mut functions = Vec::new();
    for op in block.deref(ctx).iter(ctx) {
        if let Some(import) = Operation::get_op::<ImportFuncOp>(op, ctx) {
            imports.push(import);
        } else if let Some(function) = Operation::get_op::<FuncOp>(op, ctx) {
            functions.push(function);
        }
    }

    let mut types = TypeSection::new();
    let mut type_index: HashMap<Signature, u32> = HashMap::new();
    let mut intern = |sig: &Signature| -> u32 {
        if let Some(&index) = type_index.get(sig) {
            return index;
        }
        let index = type_index.len() as u32;
        types
            .ty()
            .function(sig.0.iter().copied(), sig.1.iter().copied());
        type_index.insert(sig.clone(), index);
        index
    };

    let mut function_index: HashMap<Identifier, u32> = HashMap::new();
    let mut names = NameMap::new();
    let mut import_section = ImportSection::new();
    for (index, import) in imports.iter().enumerate() {
        let symbol = import.get_symbol_name(ctx);
        let sig = signature(ctx, import.get_type(ctx), symbol.as_ref())?;
        let ty = intern(&sig);
        import_section.import(
            &import.module(ctx),
            &import.name(ctx),
            EntityType::Function(ty),
        );
        names.append(index as u32, symbol.as_ref());
        function_index.insert(symbol, index as u32);
    }
    if let Memory::Import {
        module,
        name,
        min_pages,
    } = &options.memory
    {
        import_section.import(module, name, EntityType::Memory(memory_type(*min_pages)));
    }

    let mut function_section = FunctionSection::new();
    let mut signatures = Vec::new();
    for (offset, function) in functions.iter().enumerate() {
        let symbol = function.get_symbol_name(ctx);
        let sig = signature(ctx, function.get_type(ctx), symbol.as_ref())?;
        function_section.function(intern(&sig));
        signatures.push(sig);
        let index = (imports.len() + offset) as u32;
        names.append(index, symbol.as_ref());
        function_index.insert(symbol, index);
    }

    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut local_names = IndirectNameMap::new();
    for (offset, (function, (params, results))) in functions.iter().zip(signatures).enumerate() {
        let index = (imports.len() + offset) as u32;
        for name in export_names(ctx, function) {
            exports.export(&name, ExportKind::Func, index);
        }
        let emitted =
            FunctionEmitter::new(ctx, function, &function_index, params, results)?.emit()?;
        code.function(&emitted.body);
        local_names.append(index, &emitted.names);
    }

    let mut wasm = Module::new();
    wasm.section(&types);
    if !import_section.is_empty() {
        wasm.section(&import_section);
    }
    wasm.section(&function_section);
    if let Memory::Define { min_pages, export } = &options.memory {
        let mut memories = MemorySection::new();
        memories.memory(memory_type(*min_pages));
        wasm.section(&memories);
        if let Some(name) = export {
            exports.export(name, ExportKind::Memory, 0);
        }
    }
    if !exports.is_empty() {
        wasm.section(&exports);
    }
    wasm.section(&code);
    if !options.data.is_empty() {
        let mut data = DataSection::new();
        for segment in &options.data {
            data.active(
                0,
                &ConstExpr::i32_const(segment.offset as i32),
                segment.bytes.iter().copied(),
            );
        }
        wasm.section(&data);
    }
    if options.names {
        let mut name_section = NameSection::new();
        name_section.functions(&names);
        name_section.locals(&local_names);
        wasm.section(&name_section);
    }
    Ok(wasm.finish())
}

/// A 32-bit, unshared, unbounded memory of at least `min_pages`.
fn memory_type(min_pages: u64) -> MemoryType {
    MemoryType {
        minimum: min_pages,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    }
}
