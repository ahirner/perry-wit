//! One invocation-owned native input stream and its lazily allocated chunk buffer.

use super::StreamHelpers;
use crate::waffle_backend::{
    allocation::AllocationFuncs,
    runtime::builder::{self, Builder},
};
use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as Op,
    Type::{F64, I32},
};

const HEADER: u32 = 48;
const STATE: u32 = 0;
const STREAM: u32 = 4;
const BUFFER: u32 = 8;
const LENGTH: u32 = 12;
const ROOT: u32 = 20;
const CHUNK_SIZE: u32 = 8192;

#[repr(u32)]
enum State {
    Inactive,
    Readable,
    Closed,
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    read_transfer: Func,
    drop_stream: Func,
) -> Result<StreamHelpers> {
    let check_owner = builder::declare(module, "stream.check-owner", &[I32], &[]);
    let read_buffer = builder::declare(module, "stream.read-buffer", &[I32; 3], &[I32]);
    let helpers = StreamHelpers {
        start: builder::declare(module, "stream.start", &[I32], &[]),
        drop: builder::declare(module, "stream.drop", &[I32], &[]),
        read_chunk: builder::declare(module, "stream.read-chunk", &[I32], &[F64]),
        read_into: builder::declare(module, "stream.read-into", &[I32; 2], &[F64]),
        byte_at: builder::declare(module, "stream.byte-at", &[F64], &[F64]),
    };

    let mut b = Builder::new(module, check_owner, memory);
    let stream = b.param(0);
    let header = b.integer(HEADER);
    let state = b.load(header, STATE, I32);
    b.require(state);
    let owned = b.load(header, STREAM, I32);
    let matches = b.op(Op::I32Eq, &[owned, stream], I32);
    b.require(matches);
    b.ret(&[]);
    b.finish(module, check_owner)?;

    let mut b = Builder::new(module, helpers.start, memory);
    let stream = b.param(0);
    let header = b.integer(HEADER);
    let state = b.load(header, STATE, I32);
    let inactive = b.op(Op::I32Eqz, &[state], I32);
    b.require(inactive);
    let one = b.integer(1);
    let root = b.call(allocator.frame_new, &[one], &[I32])[0];
    let state = b.integer(State::Readable as u32);
    let zero = b.integer(0);
    for (offset, value) in [
        (STATE, state),
        (STREAM, stream),
        (BUFFER, zero),
        (LENGTH, zero),
        (ROOT, root),
    ] {
        b.store(header, offset, value, I32);
    }
    b.ret(&[]);
    b.finish(module, helpers.start)?;

    let mut b = Builder::new(module, helpers.drop, memory);
    let stream = b.param(0);
    b.call(check_owner, &[stream], &[]);
    b.call(drop_stream, &[stream], &[]);
    let header = b.integer(HEADER);
    let root = b.load(header, ROOT, I32);
    b.call(allocator.frame_drop, &[root], &[]);
    let inactive = b.integer(State::Inactive as u32);
    for offset in [STATE, STREAM, BUFFER, LENGTH, ROOT] {
        b.store(header, offset, inactive, I32);
    }
    b.ret(&[]);
    b.finish(module, helpers.drop)?;

    let mut b = Builder::new(module, read_buffer, memory);
    let stream = b.param(0);
    let data = b.param(1);
    let capacity = b.param(2);
    b.call(check_owner, &[stream], &[]);
    let header = b.integer(HEADER);
    let state = b.load(header, STATE, I32);
    let closed = b.integer(State::Closed as u32);
    let closed = b.op(Op::I32Eq, &[state, closed], I32);
    let empty = b.op(Op::I32Eqz, &[capacity], I32);
    let skip = b.op(Op::I32Or, &[closed, empty], I32);
    let done = b.body.add_block();
    let read = b.body.add_block();
    b.branch(skip, done, read);
    b.block = done;
    let zero = b.integer(0);
    b.ret(&[zero]);
    b.block = read;
    let transferred = b.call(read_transfer, &[stream, data, capacity], &[I32, I32]);
    let one = b.integer(State::Readable as u32);
    let state = b.op(Op::I32Add, &[one, transferred[1]], I32);
    b.store(header, STATE, state, I32);
    b.ret(&[transferred[0]]);
    b.finish(module, read_buffer)?;

    let mut b = Builder::new(module, helpers.read_chunk, memory);
    let stream = b.param(0);
    b.call(check_owner, &[stream], &[]);
    let header = b.integer(HEADER);
    let buffer = b.load(header, BUFFER, I32);
    let allocate = b.body.add_block();
    let read = b.body.add_block();
    b.branch(buffer, read, allocate);
    b.block = allocate;
    let buffer = b.allocate(allocator.realloc, CHUNK_SIZE, 1);
    b.store(header, BUFFER, buffer, I32);
    let root = b.load(header, ROOT, I32);
    b.store(root, 12, buffer, I32);
    b.jump(read, &[]);
    b.block = read;
    let buffer = b.load(header, BUFFER, I32);
    let capacity = b.integer(CHUNK_SIZE);
    let length = b.call(read_buffer, &[stream, buffer, capacity], &[I32])[0];
    b.store(header, LENGTH, length, I32);
    let length = b.op(Op::F64ConvertI32U, &[length], F64);
    b.ret(&[length]);
    b.finish(module, helpers.read_chunk)?;

    let mut b = Builder::new(module, helpers.read_into, memory);
    let stream = b.param(0);
    let view = b.param(1);
    let header = b.integer(HEADER);
    let zero = b.integer(0);
    b.store(header, LENGTH, zero, I32);
    let data = b.load(view, 0, I32);
    let capacity = b.load(view, 4, I32);
    let length = b.call(read_buffer, &[stream, data, capacity], &[I32])[0];
    let length = b.op(Op::F64ConvertI32U, &[length], F64);
    b.ret(&[length]);
    b.finish(module, helpers.read_into)?;

    let mut b = Builder::new(module, helpers.byte_at, memory);
    let index = b.param(0);
    let header = b.integer(HEADER);
    let state = b.load(header, STATE, I32);
    b.require(state);
    let integer = b.op(Op::F64Trunc, &[index], F64);
    let whole = b.op(Op::F64Eq, &[integer, index], I32);
    b.require(whole);
    let index = b.op(Op::I32TruncF64U, &[index], I32);
    let length = b.load(header, LENGTH, I32);
    let within = b.op(Op::I32LtU, &[index, length], I32);
    b.require(within);
    let data = b.load(header, BUFFER, I32);
    let address = b.op(Op::I32Add, &[data, index], I32);
    let byte = b.op(
        Op::I32Load8U {
            memory: b.memory(0),
        },
        &[address],
        I32,
    );
    let byte = b.op(Op::F64ConvertI32U, &[byte], F64);
    b.ret(&[byte]);
    b.finish(module, helpers.byte_at)?;
    Ok(helpers)
}
