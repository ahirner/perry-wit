//! Shared-memory output writes with explicit capability completion and ownership.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::Result;
use waffle::{Func, Memory, Module};

use crate::waffle_backend::{
    allocation::AllocationFuncs, capabilities::StdioOperation, runtime, runtime::imports,
};

pub(crate) fn declare_imports(
    module: &mut Module<'static>,
    operations: &BTreeSet<StdioOperation>,
) -> BTreeMap<String, Func> {
    imports::declare_imports(module, "output", &native_functions(operations))
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
    operations: &BTreeSet<StdioOperation>,
) -> Result<BTreeMap<String, Func>> {
    let mut wat = format!(
        "(module {}",
        imports::module_imports(&native_functions(operations))?
    );
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

fn native_functions(operations: &BTreeSet<StdioOperation>) -> Vec<imports::Function> {
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
        .map(|(name, params, results)| imports::Function {
            name: name.into(),
            params,
            results,
        })
        .collect()
}
