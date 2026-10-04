//! Shared typed loops for canonical stream transfers and partial progress.

use anyhow::Result;
use waffle::{Func, Memory, Module, Operator as Op, Type::I32};

use crate::waffle_backend::runtime::builder::{self, Builder};

/// Returns (transferred count, readable end closed). Zero-capacity reads do no I/O.
pub(crate) fn read(module: &mut Module<'static>, memory: Memory, native: Func) -> Result<Func> {
    let function = builder::declare(module, "stream.read-transfer", &[I32; 3], &[I32; 2]);
    let mut b = Builder::new(module, function, memory);
    let stream = b.param(0);
    let data = b.param(1);
    let capacity = b.param(2);
    let zero = b.integer(0);
    let one = b.integer(1);
    let mask = b.integer(15);
    let shift = b.integer(4);
    let empty = b.body.add_block();
    let read = b.body.add_block();
    let done = b.body.add_block();
    b.branch(capacity, read, empty);
    b.block = empty;
    b.ret(&[zero, zero]);
    b.block = read;
    let packed = b.call(native, &[stream, data, capacity], &[I32])[0];
    let status = b.op(Op::I32And, &[packed, mask], I32);
    let valid = b.op(Op::I32LeU, &[status, one], I32);
    b.require(valid);
    let count = b.op(Op::I32ShrU, &[packed, shift], I32);
    let in_bounds = b.op(Op::I32LeU, &[count, capacity], I32);
    b.require(in_bounds);
    let progress = b.op(Op::I32Or, &[count, status], I32);
    b.branch(progress, done, read);
    b.block = done;
    b.ret(&[count, status]);
    b.finish(module, function)?;
    Ok(function)
}

/// Returns the transferred prefix; a short prefix means the reader closed.
pub(crate) fn write(module: &mut Module<'static>, memory: Memory, native: Func) -> Result<Func> {
    let function = builder::declare(module, "stream.write-buffer", &[I32; 3], &[I32]);
    let mut b = Builder::new(module, function, memory);
    let stream = b.param(0);
    let data = b.param(1);
    let length = b.param(2);
    let zero = b.integer(0);
    let one = b.integer(1);
    let mask = b.integer(15);
    let shift = b.integer(4);
    let batch = b.integer(65536);
    let next = b.body.add_block();
    let offset = b.body.add_blockparam(next, I32);
    let transfer = b.body.add_block();
    let end = b.body.add_block();
    let transferred = b.body.add_blockparam(end, I32);
    let closed = b.body.add_block();
    let complete = b.body.add_block();
    b.jump(next, &[zero]);
    b.block = next;
    let done = b.op(Op::I32Eq, &[offset, length], I32);
    b.branch(done, complete, transfer);
    b.block = complete;
    b.jump(end, &[offset]);
    b.block = transfer;
    let remaining = b.op(Op::I32Sub, &[length, offset], I32);
    let small = b.op(Op::I32LtU, &[remaining, batch], I32);
    let requested = b.op(Op::Select, &[remaining, batch, small], I32);
    let pointer = b.op(Op::I32Add, &[data, offset], I32);
    let packed = b.call(native, &[stream, pointer, requested], &[I32])[0];
    let status = b.op(Op::I32And, &[packed, mask], I32);
    let valid = b.op(Op::I32LeU, &[status, one], I32);
    b.require(valid);
    let count = b.op(Op::I32ShrU, &[packed, shift], I32);
    let in_bounds = b.op(Op::I32LeU, &[count, requested], I32);
    b.require(in_bounds);
    let offset = b.op(Op::I32Add, &[offset, count], I32);
    let progress = b.body.add_block();
    b.branch(status, closed, progress);
    b.block = progress;
    b.jump(next, &[offset]);
    b.block = closed;
    b.jump(end, &[offset]);
    b.block = end;
    b.ret(&[transferred]);
    b.finish(module, function)?;
    Ok(function)
}
