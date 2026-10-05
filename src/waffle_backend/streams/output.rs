//! Shared-memory output writes with explicit capability completion and ownership.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as Op,
    Type::{F64, I32, I64},
};

use crate::waffle_backend::{
    allocation::AllocationFuncs,
    capabilities::StdioOperation,
    runtime::{
        builder::{self, Builder},
        imports,
    },
};

pub(crate) fn declare_imports(
    module: &mut Module<'static>,
    operations: &BTreeSet<StdioOperation>,
    owned: bool,
) -> BTreeMap<String, Func> {
    let mut functions = native_functions(operations);
    if owned {
        functions.retain(|function| !TRANSFERS.iter().any(|(name, _)| function.name == *name));
    }
    let mut native = imports::declare_imports(module, "output", &functions);
    if owned {
        native.extend(crate::waffle_backend::runtime::transfers::declare(
            module, "output", TRANSFERS,
        ));
    }
    native
}

pub(crate) const TRANSFERS: &[crate::waffle_backend::runtime::transfers::Definition] = &[
    (
        "write",
        crate::waffle_backend::runtime::transfers::Kind::Write,
    ),
    (
        "await",
        crate::waffle_backend::runtime::transfers::Kind::Completion,
    ),
];

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
    operations: &BTreeSet<StdioOperation>,
) -> Result<BTreeMap<String, Func>> {
    let write = super::transfer::write(module, memory, imports["write"])?;
    let complete = builder::declare(module, "output.complete", &[I32; 4], &[I32, F64]);
    let mut b = Builder::new(module, complete, memory);
    let writer = b.param(0);
    let completion = b.param(1);
    let owner = b.param(2);
    let newline = b.param(3);
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let three = b.integer(3);
    let frame = b.call(allocator.frame_new, &[two], &[I32])[0];
    b.store(frame, 12, owner, I32);
    let scratch = b.allocate(allocator.realloc, 3, 1);
    b.store(frame, 16, scratch, I32);
    let linefeed = b.integer(b'\n' as u32);
    b.effect(
        Op::I32Store8 {
            memory: b.memory(2),
        },
        &[scratch, linefeed],
    );
    let data = b.load(owner, 0, I32);
    let length = b.load(owner, 4, I32);
    let count = b.call(write, &[writer, data, length], &[I32])[0];
    let short = b.op(Op::I32Ne, &[count, length], I32);
    let entire = b.op(Op::I32Eqz, &[short], I32);
    let append = b.op(Op::I32And, &[newline, entire], I32);
    let append_line = b.body.add_block();
    let body_only = b.body.add_block();
    let finish = b.body.add_block();
    let incomplete = b.body.add_blockparam(finish, I32);
    b.branch(append, append_line, body_only);
    b.block = body_only;
    b.jump(finish, &[short]);
    b.block = append_line;
    let line = b.op(Op::I32Add, &[scratch, two], I32);
    let count = b.call(write, &[writer, line, one], &[I32])[0];
    let short_line = b.op(Op::I32Ne, &[count, one], I32);
    b.jump(finish, &[short_line]);
    b.block = finish;
    b.call(imports["drop-writer"], &[writer], &[]);
    let status = b.call(imports["await"], &[completion, scratch], &[I32])[0];
    let success = b.op(Op::I32Eqz, &[status], I32);
    b.require(success);
    b.call(imports["drop-future"], &[completion], &[]);
    let failed = b.op(
        Op::I32Load8U {
            memory: b.memory(0),
        },
        &[scratch],
        I32,
    );
    let code = b.op(
        Op::I32Load8U {
            memory: b.memory(1),
        },
        &[scratch],
        I32,
    );
    let code = b.op(Op::I32Add, &[code, one], I32);
    let short_error = b.op(Op::Select, &[three, zero, incomplete], I32);
    let error = b.op(Op::Select, &[code, short_error, failed], I32);
    b.call(allocator.frame_drop, &[frame], &[]);
    let failed = b.op(Op::I32Ne, &[error, zero], I32);
    let payload = b.op(Op::F64ConvertI32U, &[error], F64);
    b.ret(&[failed, payload]);
    b.finish(module, complete)?;

    let mut functions = BTreeMap::new();
    for operation in operations {
        let function = builder::declare(module, operation.name(), &[I32], &[I32, F64]);
        let mut b = Builder::new(module, function, memory);
        let pair = b.call(imports["new"], &[], &[I64])[0];
        let reader = b.op(Op::I32WrapI64, &[pair], I32);
        let completion = b.call(imports[operation.channel()], &[reader], &[I32])[0];
        let shift = b.op(Op::I64Const { value: 32 }, &[], I64);
        let writer = b.op(Op::I64ShrU, &[pair, shift], I64);
        let writer = b.op(Op::I32WrapI64, &[writer], I32);
        let newline = b.integer(u32::from(operation.newline()));
        let result = b.call(
            complete,
            &[writer, completion, b.param(0), newline],
            &[I32, F64],
        );
        b.ret(&result);
        b.finish(module, function)?;
        functions.insert(operation.name().into(), function);
    }
    Ok(functions)
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
