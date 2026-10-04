//! Selective P3 process context imports with instance-retained guest snapshots.

use super::{
    allocation::AllocationFuncs, capabilities::ContextOperation, component::forward,
    objects::ObjectHelpers, runtime, structured::StructuredHelpers,
};
use anyhow::Result;
use perry_hir::types::Type as HirType;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};
use waffle::{Func, Memory, Module};

pub(crate) const ENVIRONMENT_TYPE: &str = "__perry_internal_environment";

pub(crate) fn is_environment(ty: &HirType) -> bool {
    matches!(ty,HirType::Named(name) if name == ENVIRONMENT_TYPE)
}

pub(crate) struct ContextHelpers {
    pub(crate) string_lift: Func,
    pub(crate) structured: Option<StructuredHelpers>,
    pub(crate) objects: Option<ObjectHelpers>,
}

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
    helpers: ContextHelpers,
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
            helpers
                .structured
                .expect("arguments require string array helpers")
                .take_strings,
        );
    }
    if operations.contains(&ContextOperation::InitialCwd)
        || operations.contains(&ContextOperation::Environment)
    {
        wat.push_str("(import \"host\" \"string\" (func $string (param i32 i32) (result i32)))");
        imports.insert("string", helpers.string_lift);
    }
    if operations.contains(&ContextOperation::Environment) {
        wat.push_str("(import \"host\" \"environment\" (func $environment (result i32))) (import \"host\" \"set\" (func $set (param i32 i32 i32 f64) (result i32 f64)))");
        let objects = helpers.objects.expect("environment requires objects");
        imports.insert("environment", objects.new);
        imports.insert("set", objects.set);
    }
    wat.push_str(include_str!("context/cache.wat"));
    if operations.contains(&ContextOperation::Environment) {
        wat.push_str(include_str!("context/environment.wat"));
    }
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
            ContextOperation::Environment => "(list (tuple string string))",
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
