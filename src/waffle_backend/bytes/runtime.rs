//! Byte-view identity, shared backing storage, and checked numeric indexing.

use super::ByteHelpers;
use crate::waffle_backend::{
    allocation::AllocationFuncs,
    runtime::builder::{self, Builder},
};
use anyhow::Result;
use waffle::{
    Memory, Module, Operator as Op,
    Type::{F64, I32},
};

const DATA: u32 = 0;
const LENGTH: u32 = 4;
const OWNER: u32 = 8;
const OFFSET: u32 = 12;

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
) -> Result<ByteHelpers> {
    let view = builder::declare(module, "bytes.view", &[I32, I32, I32], &[I32]);
    let allocate = builder::declare(module, "bytes.allocate", &[I32], &[I32]);
    let valid_index = builder::declare(module, "bytes.valid-index", &[I32, F64], &[I32]);
    let bound = builder::declare(module, "bytes.bound", &[F64, I32], &[I32]);
    let helpers = ByteHelpers {
        lift_canonical: builder::declare(module, "bytes.lift", &[I32, I32], &[I32]),
        new: builder::declare(module, "bytes.new", &[F64], &[I32, F64]),
        copy: builder::declare(module, "bytes.copy", &[I32], &[I32]),
        get: builder::declare(module, "bytes.get", &[I32, F64], &[F64]),
        set: builder::declare(module, "bytes.set", &[I32, F64, F64], &[]),
        subarray: builder::declare(module, "bytes.subarray", &[I32, F64, F64], &[I32]),
    };

    let mut b = Builder::new(module, view, memory);
    let owner = b.param(0);
    let offset = b.param(1);
    let length = b.param(2);
    let descriptor = b.allocate(allocator.realloc, 16, 4);
    let four = b.integer(4);
    let header = b.op(Op::I32Sub, &[descriptor, four], I32);
    let header = b.load(header, 0, I32);
    let kind = b.integer(7);
    b.store(header, 16, kind, I32);
    let data = b.op(Op::I32Add, &[owner, offset], I32);
    for (offset, value) in [
        (DATA, data),
        (LENGTH, length),
        (OWNER, owner),
        (OFFSET, offset),
    ] {
        b.store(descriptor, offset, value, I32);
    }
    b.ret(&[descriptor]);
    b.finish(module, view)?;

    let mut b = Builder::new(module, allocate, memory);
    let length = b.param(0);
    let zero = b.integer(0);
    let one = b.integer(1);
    let size = b.op(Op::Select, &[length, one, length], I32);
    let owner = b.call(allocator.realloc, &[zero, zero, one, size], &[I32])[0];
    b.effect(Op::MemoryFill { mem: memory }, &[owner, zero, length]);
    let result = b.call(view, &[owner, zero, length], &[I32])[0];
    b.ret(&[result]);
    b.finish(module, allocate)?;

    let mut b = Builder::new(module, helpers.lift_canonical, memory);
    let data = b.param(0);
    let length = b.param(1);
    let zero = b.integer(0);
    let result = b.call(view, &[data, zero, length], &[I32])[0];
    b.ret(&[result]);
    b.finish(module, helpers.lift_canonical)?;

    let mut b = Builder::new(module, helpers.new, memory);
    let size = b.param(0);
    let zero = b.number(0.0);
    let maximum = b.number(u32::MAX as f64);
    let ordered = b.op(Op::F64Eq, &[size, size], I32);
    let size = b.op(Op::Select, &[size, zero, ordered], F64);
    let size = b.op(Op::F64Trunc, &[size], F64);
    let low = b.op(Op::F64Ge, &[size, zero], I32);
    let high = b.op(Op::F64Le, &[size, maximum], I32);
    let valid = b.op(Op::I32And, &[low, high], I32);
    let accepted = b.body.add_block();
    let rejected = b.body.add_block();
    b.branch(valid, accepted, rejected);
    b.block = rejected;
    let status = b.integer(1);
    let error = b.number(1.0);
    b.ret(&[status, error]);
    b.block = accepted;
    let size = b.op(Op::I32TruncF64U, &[size], I32);
    let result = b.call(allocate, &[size], &[I32])[0];
    let payload = b.op(Op::F64ConvertI32U, &[result], F64);
    let success = b.integer(0);
    b.ret(&[success, payload]);
    b.finish(module, helpers.new)?;

    let mut b = Builder::new(module, helpers.copy, memory);
    let source = b.param(0);
    let length = b.load(source, LENGTH, I32);
    let result = b.call(allocate, &[length], &[I32])[0];
    let destination = b.load(result, DATA, I32);
    let source = b.load(source, DATA, I32);
    b.effect(
        Op::MemoryCopy {
            src_mem: memory,
            dst_mem: memory,
        },
        &[destination, source, length],
    );
    b.ret(&[result]);
    b.finish(module, helpers.copy)?;

    let mut b = Builder::new(module, valid_index, memory);
    let descriptor = b.param(0);
    let index = b.param(1);
    let length = b.load(descriptor, LENGTH, I32);
    let length = b.op(Op::F64ConvertI32U, &[length], F64);
    let integer = b.op(Op::F64Trunc, &[index], F64);
    let whole = b.op(Op::F64Eq, &[integer, index], I32);
    let zero = b.number(0.0);
    let nonnegative = b.op(Op::F64Ge, &[index, zero], I32);
    let within = b.op(Op::F64Lt, &[index, length], I32);
    let valid = b.op(Op::I32And, &[nonnegative, within], I32);
    let valid = b.op(Op::I32And, &[whole, valid], I32);
    b.ret(&[valid]);
    b.finish(module, valid_index)?;

    for (function, write) in [(helpers.get, false), (helpers.set, true)] {
        let mut b = Builder::new(module, function, memory);
        let descriptor = b.param(0);
        let index = b.param(1);
        let valid = b.call(valid_index, &[descriptor, index], &[I32])[0];
        let access = b.body.add_block();
        let missing = b.body.add_block();
        b.branch(valid, access, missing);
        b.block = missing;
        let result = if write {
            vec![]
        } else {
            vec![b.number(f64::NAN)]
        };
        b.ret(&result);
        b.block = access;
        let data = b.load(descriptor, DATA, I32);
        let index = b.op(Op::I32TruncF64U, &[index], I32);
        let address = b.op(Op::I32Add, &[data, index], I32);
        if write {
            let value = b.param(2);
            let magnitude = b.op(Op::F64Abs, &[value], F64);
            let infinity = b.number(f64::INFINITY);
            let finite = b.op(Op::F64Lt, &[magnitude, infinity], I32);
            let zero = b.number(0.0);
            let value = b.op(Op::Select, &[value, zero, finite], F64);
            let integer = b.op(Op::F64Trunc, &[value], F64);
            let modulus = b.number(256.0);
            let quotient = b.op(Op::F64Div, &[integer, modulus], F64);
            let quotient = b.op(Op::F64Floor, &[quotient], F64);
            let multiple = b.op(Op::F64Mul, &[quotient, modulus], F64);
            let remainder = b.op(Op::F64Sub, &[integer, multiple], F64);
            let byte = b.op(Op::I32TruncF64U, &[remainder], I32);
            b.effect(
                Op::I32Store8 {
                    memory: b.memory(0),
                },
                &[address, byte],
            );
            b.ret(&[]);
        } else {
            let byte = b.op(
                Op::I32Load8U {
                    memory: b.memory(0),
                },
                &[address],
                I32,
            );
            let value = b.op(Op::F64ConvertI32U, &[byte], F64);
            b.ret(&[value]);
        }
        b.finish(module, function)?;
    }

    let mut b = Builder::new(module, bound, memory);
    let position = b.param(0);
    let length = b.param(1);
    let length = b.op(Op::F64ConvertI32U, &[length], F64);
    let zero = b.number(0.0);
    let ordered = b.op(Op::F64Eq, &[position, position], I32);
    let position = b.op(Op::Select, &[position, zero, ordered], F64);
    let position = b.op(Op::F64Trunc, &[position], F64);
    let negative = b.op(Op::F64Lt, &[position, zero], I32);
    let relative = b.op(Op::F64Add, &[length, position], F64);
    let position = b.op(Op::Select, &[relative, position, negative], F64);
    let position = b.op(Op::F64Max, &[position, zero], F64);
    let position = b.op(Op::F64Min, &[position, length], F64);
    let result = b.op(Op::I32TruncF64U, &[position], I32);
    b.ret(&[result]);
    b.finish(module, bound)?;

    let mut b = Builder::new(module, helpers.subarray, memory);
    let source = b.param(0);
    let start = b.param(1);
    let end = b.param(2);
    let length = b.load(source, LENGTH, I32);
    let begin = b.call(bound, &[start, length], &[I32])[0];
    let end = b.call(bound, &[end, length], &[I32])[0];
    let owner = b.load(source, OWNER, I32);
    let offset = b.load(source, OFFSET, I32);
    let offset = b.op(Op::I32Add, &[offset, begin], I32);
    let nonempty = b.op(Op::I32GtU, &[end, begin], I32);
    let size = b.op(Op::I32Sub, &[end, begin], I32);
    let zero = b.integer(0);
    let size = b.op(Op::Select, &[size, zero, nonempty], I32);
    let result = b.call(view, &[owner, offset, size], &[I32])[0];
    b.ret(&[result]);
    b.finish(module, helpers.subarray)?;
    Ok(helpers)
}
