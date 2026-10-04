//! Selective P3 process context imports with instance-retained guest snapshots.

use super::{
    allocation::AllocationFuncs, capabilities::ContextOperation, component::forward, runtime,
    structured::StructuredHelpers,
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};
use waffle::{Func, Memory, Module};

pub(crate) fn declare_imports(
    module: &mut Module<'static>,
    operations: &BTreeSet<ContextOperation>,
) -> BTreeMap<String, Func> {
    forward::declare_imports(module, "context", &native_functions(operations))
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
    operations: &BTreeSet<ContextOperation>,
    string_lift: Func,
    structured: Option<StructuredHelpers>,
) -> Result<BTreeMap<String, Func>> {
    let mut wat = format!(
        "(module {} (import \"host\" \"realloc\" (func $realloc (param i32 i32 i32 i32) (result i32))) (import \"host\" \"retain\" (func $retain (param i32) (result i32)))",
        forward::module_imports(&native_functions(operations))?
    );
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &function)| (name.as_str(), function))
        .collect();
    imports.insert("realloc", allocator.realloc);
    imports.insert("retain", allocator.retained_frame_new);
    if operations.contains(&ContextOperation::Arguments) {
        wat.push_str("(import \"host\" \"strings\" (func $strings (param i32 i32) (result i32)))");
        imports.insert(
            "strings",
            structured
                .expect("arguments require string array helpers")
                .take_strings,
        );
    }
    if operations.contains(&ContextOperation::InitialCwd) {
        wat.push_str("(import \"host\" \"string\" (func $string (param i32 i32) (result i32)))");
        imports.insert("string", string_lift);
    }
    wat.push_str(include_str!("context/cache.wat"));
    if operations.contains(&ContextOperation::Arguments) {
        wat.push_str(include_str!("context/arguments.wat"));
    }
    if operations.contains(&ContextOperation::InitialCwd) {
        wat.push_str(include_str!("context/cwd.wat"));
    }
    wat.push(')');
    runtime::emit_functions(module, memory, &wat, &imports)
}

pub(crate) fn declare_adapters(operations: &BTreeSet<ContextOperation>) -> Result<String> {
    if operations.is_empty() {
        return Ok(String::new());
    }
    let mut wat = String::from("(import \"wasi:cli/environment@0.3.0\" (instance $context");
    for operation in operations {
        let result = match operation {
            ContextOperation::Arguments => "(list string)",
            ContextOperation::InitialCwd => "(option string)",
        };
        write!(
            wat,
            "(export {:?} (func (result {result})))",
            operation.import()
        )?;
    }
    wat.push_str("))");
    wat.push_str(&forward::declare("context", &native_functions(operations))?);
    Ok(wat)
}

pub(crate) fn bind_adapters(operations: &BTreeSet<ContextOperation>) -> Result<String> {
    let mut wat = String::new();
    for operation in operations {
        writeln!(
            wat,
            "(core func $context-{} (canon lower (func $context {:?}) (memory (core memory $guest \"memory\")) (realloc (core func $guest \"cabi_realloc\"))))",
            operation.import(),
            operation.import()
        )?;
    }
    wat.push_str(&forward::bind("context", &native_functions(operations))?);
    Ok(wat)
}

fn native_functions(operations: &BTreeSet<ContextOperation>) -> Vec<forward::Function> {
    operations
        .iter()
        .map(|operation| forward::Function {
            name: operation.import().into(),
            params: vec!["i32"],
            results: vec![],
            target: format!("(func $context-{})", operation.import()),
        })
        .collect()
}
