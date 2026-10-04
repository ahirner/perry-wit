//! Selective P3 random imports and shared-memory byte filling.

use super::{
    allocation::AllocationFuncs,
    capabilities::{RandomOperation, scalars::Scalar},
    component::forward,
    runtime,
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use waffle::{Func, Memory, Module};

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    forward::declare_imports(module, "random", &native_functions())
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
) -> Result<BTreeMap<String, Func>> {
    let wat = format!(
        "(module {} (import \"host\" \"realloc\" (func $realloc (param i32 i32 i32 i32) (result i32))) {})",
        forward::module_imports(&native_functions())?,
        include_str!("random/runtime.wat")
    );
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &function)| (name.as_str(), function))
        .collect();
    imports.insert("realloc", allocator.realloc);
    runtime::emit_functions(module, memory, &wat, &imports)
}

pub(crate) fn declare_adapters(operations: &BTreeSet<RandomOperation>) -> Result<String> {
    if operations.is_empty() {
        return Ok(String::new());
    }
    let bytes = operations.iter().any(|operation| operation.needs_bytes());
    let number = operations.contains(&RandomOperation::Number);
    let mut wat = String::from("(import \"wasi:random/random@0.3.0\" (instance $random");
    if number {
        wat.push_str("(export \"get-random-u64\" (func (result u64)))");
    }
    if bytes {
        wat.push_str(
            "(export \"get-random-bytes\" (func (param \"max-len\" u64) (result (list u8))))",
        );
    }
    wat.push_str("))");
    if number {
        let body = Scalar::Random.body("sample", "word");
        wat.push_str(&format!(r#"
          (core func $random-word (canon lower (func $random "get-random-u64")))
          (core module $random-number
            (import "native" "word" (func $word (result i64)))
            {body})
          (core instance $random-number (instantiate $random-number (with "native" (instance (export "word" (func $random-word))))))
        "#));
    }
    if bytes {
        wat.push_str(&forward::declare("random", &native_functions())?);
    }
    Ok(wat)
}

pub(crate) fn bind_adapters() -> Result<String> {
    Ok(format!(
        r#"
      (core func $random-bytes (canon lower (func $random "get-random-bytes")
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      {}"#,
        forward::bind("random", &native_functions())?
    ))
}

fn native_functions() -> Vec<forward::Function> {
    vec![forward::Function {
        name: "bytes".into(),
        params: vec!["i64", "i32"],
        results: vec![],
        target: "(func $random-bytes)".into(),
    }]
}
