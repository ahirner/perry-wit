//! Shared backing buffers own bytes; views are weakly linked for atomic detachment.

use crate::waffle_backend::{
    allocation::AllocationFuncs,
    runtime::builder::{self, Builder},
};
use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as Op,
    Type::{F64, I32},
};

pub(super) const DETACHED: u32 = 8;
pub(super) const VIEWS: u32 = 12;
pub(super) const NEXT: u32 = 16;
pub(super) const PREVIOUS: u32 = 20;

pub(super) struct Helpers {
    pub new: Func,
    pub from_buffer: Func,
    pub validate: Func,
    pub transfer: Func,
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    view: Func,
) -> Result<Helpers> {
    let h = Helpers {
        new: builder::declare(module, "buffer.new", &[I32; 2], &[I32]),
        from_buffer: builder::declare(module, "bytes.from-buffer", &[I32], &[I32, F64]),
        validate: builder::declare(module, "bytes.validate", &[I32], &[I32, F64]),
        transfer: builder::declare(module, "bytes.transfer", &[I32], &[I32]),
    };
    let mut b = Builder::new(module, h.new, memory);
    let buffer = b.allocate(allocator.realloc, 16, 4);
    let four = b.integer(4);
    let backlink = b.op(Op::I32Sub, &[buffer, four], I32);
    let header = b.load(backlink, 0, I32);
    let kind = b.integer(20);
    b.store(header, 16, kind, I32);
    let zero = b.integer(0);
    b.store(buffer, 0, b.param(0), I32);
    b.store(buffer, 4, b.param(1), I32);
    b.store(buffer, DETACHED, zero, I32);
    b.store(buffer, VIEWS, zero, I32);
    b.ret(&[buffer]);
    b.finish(module, h.new)?;

    for (function, from_buffer) in [(h.from_buffer, true), (h.validate, false)] {
        let mut b = Builder::new(module, function, memory);
        let buffer = if from_buffer {
            b.param(0)
        } else {
            b.load(b.param(0), 8, I32)
        };
        let detached = b.load(buffer, DETACHED, I32);
        let reject = b.body.add_block();
        let ready = b.body.add_block();
        b.branch(detached, reject, ready);
        b.block = reject;
        let one = b.integer(1);
        let error = b.number(12.0);
        b.ret(&[one, error]);
        b.block = ready;
        let zero = b.integer(0);
        let value = if from_buffer {
            let length = b.load(buffer, 4, I32);
            let result = b.call(view, &[buffer, zero, length], &[I32])[0];
            b.op(Op::F64ConvertI32U, &[result], F64)
        } else {
            b.number(0.0)
        };
        b.ret(&[zero, value]);
        b.finish(module, function)?;
    }

    let mut b = Builder::new(module, h.transfer, memory);
    let source = b.param(0);
    let owner = b.load(source, 8, I32);
    let data = b.load(owner, 0, I32);
    let capacity = b.load(owner, 4, I32);
    let offset = b.load(source, 12, I32);
    let length = b.load(source, 4, I32);
    let buffer = b.call(h.new, &[data, capacity], &[I32])[0];
    let result = b.call(view, &[buffer, offset, length], &[I32])[0];
    let zero = b.integer(0);
    let one = b.integer(1);
    b.store(owner, 0, zero, I32);
    b.store(owner, 4, zero, I32);
    b.store(owner, DETACHED, one, I32);
    let head = b.load(owner, VIEWS, I32);
    let next = b.body.add_block();
    let alias = b.body.add_blockparam(next, I32);
    let detach = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[head]);
    b.block = next;
    b.branch(alias, detach, done);
    b.block = detach;
    for offset in [0, 4, 12] {
        b.store(alias, offset, zero, I32);
    }
    let following = b.load(alias, NEXT, I32);
    b.jump(next, &[following]);
    b.block = done;
    b.ret(&[result]);
    b.finish(module, h.transfer)?;
    Ok(h)
}
