//! Canonical subtask ownership, distinct from retained source Promise outcomes.

use anyhow::Result;
use waffle::{Func, Memory, Module, Operator as Op, Type::I32};

use super::builder::{self, Builder};
use crate::waffle_backend::allocation::AllocationFuncs;

pub(crate) struct SubtaskImports {
    new_set: Func,
    drop_set: Func,
    join: Func,
    wait: Func,
    drop: Func,
}

pub(crate) fn declare(module: &mut Module<'static>) -> SubtaskImports {
    SubtaskImports {
        new_set: builder::native(module, "[waitable-set-new]", &[], &[I32]),
        drop_set: builder::native(module, "[waitable-set-drop]", &[I32], &[]),
        join: builder::native(module, "[waitable-join]", &[I32, I32], &[]),
        wait: builder::native(module, "[waitable-set-wait]", &[I32, I32], &[I32]),
        drop: builder::native(module, "[subtask-drop]", &[I32], &[]),
    }
}

/// Consumes a canonical async-lower result and returns its terminal status.
/// The caller retains arguments and result storage throughout this wait.
pub(crate) fn emit_wait(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: SubtaskImports,
) -> Result<Func> {
    let SubtaskImports {
        new_set,
        drop_set,
        join,
        wait,
        drop,
    } = imports;
    let function = builder::declare(module, "subtask.wait", &[I32], &[I32]);
    let mut b = Builder::new(module, function, memory);
    let packed = b.param(0);
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let four = b.integer(4);
    let mask = b.integer(15);
    let status = b.op(Op::I32And, &[packed, mask], I32);
    let valid = b.op(Op::I32LeU, &[status, two], I32);
    b.require(valid);
    let returned = b.op(Op::I32Eq, &[status, two], I32);
    let immediate = b.body.add_block();
    let pending = b.body.add_block();
    b.branch(returned, immediate, pending);
    b.block = immediate;
    b.ret(&[two]);
    b.block = pending;
    let handle = b.op(Op::I32ShrU, &[packed, four], I32);
    let frame = b.call(allocator.frame_new, &[one], &[I32])[0];
    let event = b.allocate(allocator.realloc, 8, 4);
    b.store(frame, 12, event, I32);
    let set = b.call(new_set, &[], &[I32])[0];
    b.call(join, &[handle, set], &[]);
    let receive = b.body.add_block();
    let terminal = b.body.add_block();
    b.jump(receive, &[]);
    b.block = receive;
    let kind = b.call(wait, &[set, event], &[I32])[0];
    let subtask = b.op(Op::I32Eq, &[kind, one], I32);
    b.require(subtask);
    let notified = b.load(event, 0, I32);
    let same = b.op(Op::I32Eq, &[notified, handle], I32);
    b.require(same);
    let status = b.load(event, 4, I32);
    let valid = b.op(Op::I32LeU, &[status, four], I32);
    b.require(valid);
    let done = b.op(Op::I32GeU, &[status, two], I32);
    b.branch(done, terminal, receive);
    b.block = terminal;
    b.call(join, &[handle, zero], &[]);
    b.call(drop, &[handle], &[]);
    b.call(drop_set, &[set], &[]);
    b.call(allocator.frame_drop, &[frame], &[]);
    b.ret(&[status]);
    b.finish(module, function)?;
    Ok(function)
}
