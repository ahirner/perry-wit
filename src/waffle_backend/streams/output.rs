//! Shared-memory output writes with explicit capability completion and ownership.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::Result;
use waffle::{Func, FuncDecl, Import, ImportKind, Memory, Module, SignatureData, Type};

use crate::waffle_backend::{
    allocation::AllocationFuncs, capabilities::StdioOperation, component::forward, runtime,
};

pub(crate) fn declare_imports(
    module: &mut Module<'static>,
    operations: &BTreeSet<StdioOperation>,
) -> BTreeMap<String, Func> {
    native_functions(operations)
        .into_iter()
        .map(|function| {
            let core_type = |name| match name {
                "i32" => Type::I32,
                "i64" => Type::I64,
                _ => unreachable!("output imports use integer core handles"),
            };
            let signature = module.signatures.push(SignatureData {
                params: function.params.into_iter().map(core_type).collect(),
                returns: function.results.into_iter().map(core_type).collect(),
            });
            let index = module
                .funcs
                .push(FuncDecl::Import(signature, function.name.clone()));
            module.imports.push(Import {
                module: "output".into(),
                name: function.name.clone(),
                kind: ImportKind::Func(index),
            });
            (function.name, index)
        })
        .collect()
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
    operations: &BTreeSet<StdioOperation>,
) -> Result<BTreeMap<String, Func>> {
    let mut wat = String::from("(module (import \"host\" \"memory\" (memory 1))\n");
    for function in native_functions(operations) {
        writeln!(
            wat,
            "(import \"host\" {0:?} (func ${0} (param {1}) (result {2})))",
            function.name,
            function.params.join(" "),
            function.results.join(" ")
        )?;
    }
    wat.push_str(
        r#"(import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
        (import "host" "frame-new" (func $frame-new (param i32) (result i32)))
        (import "host" "frame-drop" (func $frame-drop (param i32)))
        "#,
    );
    wat.push_str(include_str!("write.wat"));
    wat.push_str(include_str!("output.wat"));
    for &operation in operations {
        writeln!(
            wat,
            r#"(func (export {:?}) (param $owner i32) (result i32 f64)
              (local $pair i64) (local $completion i32)
              (local.set $pair (call $new))
              (local.set $completion (call ${} (i32.wrap_i64 (local.get $pair))))
              (call $complete-output
                (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32)))
                (local.get $completion) (local.get $owner) (i32.const {})))"#,
            operation.name(),
            operation.channel(),
            u8::from(operation.newline())
        )?;
    }
    wat.push(')');
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &index)| (name.as_str(), index))
        .collect();
    imports.extend([
        ("realloc", allocator.realloc),
        ("frame-new", allocator.frame_new),
        ("frame-drop", allocator.frame_drop),
    ]);
    runtime::emit_functions(module, memory, &wat, &imports)
}

pub(crate) fn declare_adapters(operations: &BTreeSet<StdioOperation>) -> Result<String> {
    if operations.is_empty() {
        return Ok(String::new());
    }
    let mut wat = String::from(
        r#"(import "wasi:cli/types@0.3.0" (instance $stdio-types
          (type $error-base (enum "io" "illegal-byte-sequence" "pipe"))
          (export "error-code" (type (eq $error-base)))))
        (alias export $stdio-types "error-code" (type $stdio-error))
        (type $output-bytes (stream u8))
        (type $output-completion (future (result (error $stdio-error))))
        "#,
    );
    for channel in operations
        .iter()
        .map(|operation| operation.channel())
        .collect::<BTreeSet<_>>()
    {
        writeln!(
            wat,
            r#"(import "wasi:cli/{channel}@0.3.0" (instance ${channel}
              (alias outer 1 $stdio-error (type $error))
              (export "error-code" (type (eq $error)))
              (type $bytes (stream u8))
              (type $completion (future (result (error $error))))
              (export "write-via-stream" (func (param "data" $bytes) (result $completion)))))
            (core func $output-{channel} (canon lower (func ${channel} "write-via-stream")))"#
        )?;
    }
    wat.push_str(&forward::declare("output", &native_functions(operations))?);
    Ok(wat)
}

pub(crate) fn bind_adapters(operations: &BTreeSet<StdioOperation>) -> Result<String> {
    if operations.is_empty() {
        return Ok(String::new());
    }
    let mut wat = String::from(
        r#"(core func $output-new (canon stream.new $output-bytes))
        (core func $output-write (canon stream.write $output-bytes (memory (core memory $guest "memory"))))
        (core func $output-drop-writer (canon stream.drop-writable $output-bytes))
        (core func $output-await (canon future.read $output-completion (memory (core memory $guest "memory"))))
        (core func $output-drop-future (canon future.drop-readable $output-completion))
        "#,
    );
    wat.push_str(&forward::bind("output", &native_functions(operations))?);
    Ok(wat)
}

fn native_functions(operations: &BTreeSet<StdioOperation>) -> Vec<forward::Function> {
    let mut functions = vec![
        ("new", vec![], vec!["i64"]),
        ("write", vec!["i32"; 3], vec!["i32"]),
        ("drop-writer", vec!["i32"], vec![]),
        ("await", vec!["i32"; 2], vec!["i32"]),
        ("drop-future", vec!["i32"], vec![]),
    ];
    for channel in operations
        .iter()
        .map(|operation| operation.channel())
        .collect::<BTreeSet<_>>()
    {
        functions.push((channel, vec!["i32"], vec!["i32"]));
    }
    functions
        .into_iter()
        .map(|(name, params, results)| forward::Function {
            name: name.into(),
            params,
            results,
            target: format!("(func $output-{name})"),
        })
        .collect()
}
