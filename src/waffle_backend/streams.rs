//! Native byte transfers into managed buffers owned by one active entry invocation.

pub(crate) mod buffered;
pub(crate) mod output;

#[cfg(test)]
#[path = "streams/buffered_test.rs"]
mod buffered_test;

use std::collections::BTreeMap;

use anyhow::Result;
use waffle::{Func, FuncDecl, Import, ImportKind, Memory, Module, SignatureData, Type};

use super::{allocation::AllocationFuncs, component::forward, runtime};

pub(crate) struct StreamImports {
    read: Func,
    drop: Func,
}

#[derive(Clone, Copy)]
pub(crate) struct StreamHelpers {
    pub(crate) start: Func,
    pub(crate) drop: Func,
    pub(crate) read_chunk: Func,
    pub(crate) read_into: Func,
    pub(crate) byte_at: Func,
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> StreamImports {
    let mut declare = |name: &str, params: Vec<Type>, returns: Vec<Type>| {
        let signature = module.signatures.push(SignatureData { params, returns });
        let function = module.funcs.push(FuncDecl::Import(signature, name.into()));
        module.imports.push(Import {
            module: "streams".into(),
            name: name.into(),
            kind: ImportKind::Func(function),
        });
        function
    };
    StreamImports {
        read: declare("read", vec![Type::I32; 3], vec![Type::I32]),
        drop: declare("drop", vec![Type::I32], vec![]),
    }
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: StreamImports,
) -> Result<StreamHelpers> {
    let transfer = emit_read_transfer(module, memory, imports.read)?;
    let functions = runtime::emit_functions(
        module,
        memory,
        include_str!("streams/runtime.wat"),
        &BTreeMap::from([
            ("read-transfer", transfer),
            ("drop", imports.drop),
            ("realloc", allocator.realloc),
            ("frame-new", allocator.frame_new),
            ("frame-drop", allocator.frame_drop),
        ]),
    )?;
    Ok(StreamHelpers {
        start: functions["stream.start"],
        drop: functions["stream.drop"],
        read_chunk: functions["stream.read-chunk"],
        read_into: functions["stream.read-into"],
        byte_at: functions["stream.byte-at"],
    })
}

pub(crate) fn emit_read_transfer(
    module: &mut Module<'static>,
    memory: Memory,
    read: Func,
) -> Result<Func> {
    let functions = runtime::emit_functions(
        module,
        memory,
        &format!(
            "(module (import \"host\" \"read\" (func $read (param i32 i32 i32) (result i32))) (memory 1) {} (export \"read-transfer\" (func $read-transfer)))",
            include_str!("streams/read.wat")
        ),
        &BTreeMap::from([("read", read)]),
    )?;
    Ok(functions["read-transfer"])
}

pub(crate) fn forward_functions() -> Vec<forward::Function> {
    vec![
        forward::Function {
            name: "read".into(),
            params: vec!["i32"; 3],
            results: vec!["i32"],
            target: "(func $stream-read)".into(),
        },
        forward::Function {
            name: "drop".into(),
            params: vec!["i32"],
            results: vec![],
            target: "(func $stream-drop)".into(),
        },
    ]
}
