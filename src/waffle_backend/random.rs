//! Selective P3 random imports and shared-memory byte filling.

use super::{allocation::AllocationFuncs, runtime, runtime::imports};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    imports::declare_imports(module, "random", &native_functions())
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
) -> Result<BTreeMap<String, Func>> {
    let wat = format!(
        "(module {} (import \"host\" \"realloc\" (func $realloc (param i32 i32 i32 i32) (result i32))) {})",
        imports::module_imports(&native_functions())?,
        include_str!("random/runtime.wat")
    );
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &function)| (name.as_str(), function))
        .collect();
    imports.insert("realloc", allocator.realloc);
    runtime::emit_functions(module, memory, &wat, &imports)
}

fn native_functions() -> Vec<imports::Function> {
    vec![imports::Function {
        name: "bytes".into(),
        params: vec!["i64", "i32"],
        results: vec![],
    }]
}
