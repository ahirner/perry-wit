//! Fill transferred byte views directly while preserving queued input and offsets.
use super::*;

pub(in crate::waffle_backend::streams) fn destination(
    b: &mut Builder,
    r: &Runtime<'_>,
    provided: Value,
    default_capacity: u32,
) -> (Value, Value, Value) {
    if !r.readers.has_default() {
        return (provided, b.load(provided, 0, I32), b.load(provided, 4, I32));
    }
    let allocate = b.body.add_block();
    let supplied = b.body.add_block();
    let ready = b.body.add_block();
    let view = b.body.add_blockparam(ready, I32);
    if r.readers.has_byob() {
        b.branch(provided, supplied, allocate);
    } else {
        b.jump(allocate, &[]);
    }
    b.block = supplied;
    b.jump(ready, &[provided]);
    b.block = allocate;
    let data = b.allocate(r.allocator.realloc, default_capacity, 1);
    let capacity = b.integer(default_capacity);
    let created = b.call(r.bytes.lift_canonical, &[data, capacity], &[I32])[0];
    b.jump(ready, &[created]);
    b.block = ready;
    let data = b.load(view, 0, I32);
    let capacity = b.load(view, 4, I32);
    (view, data, capacity)
}

pub(super) fn emit_copy_chunk(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
) -> Result<Func> {
    let f = builder::declare(module, "web.copy-chunk", &[I32; 3], &[I32]);
    let mut b = Builder::new(module, f, memory);
    let stream = b.param(0);
    let source = b.param(1);
    let destination = b.param(2);
    if r.readers.has_default() {
        let copy = b.body.add_block();
        let direct = b.body.add_block();
        b.branch(destination, copy, direct);
        b.block = direct;
        b.ret(&[source]);
        b.block = copy;
    }
    let available = b.load(source, 4, I32);
    let capacity = b.load(destination, 4, I32);
    let partial = b.op(O::I32GtU, &[available, capacity], I32);
    let length = b.op(O::Select, &[capacity, available, partial], I32);
    let from = b.load(source, 0, I32);
    let to = b.load(destination, 0, I32);
    b.effect(
        O::MemoryCopy {
            src_mem: memory,
            dst_mem: memory,
        },
        &[to, from, length],
    );
    b.store(destination, 4, length, I32);
    let remainder = b.body.add_block();
    let done = b.body.add_block();
    b.branch(partial, remainder, done);
    b.block = remainder;
    let start = b.op(O::F64ConvertI32U, &[length], F64);
    let end = b.op(O::F64ConvertI32U, &[available], F64);
    let rest = b.call(r.bytes.subarray, &[source, start, end], &[I32])[0];
    b.store(stream, PENDING_CHUNK, rest, I32);
    b.jump(done, &[]);
    b.block = done;
    b.ret(&[destination]);
    b.finish(module, f)?;
    Ok(f)
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    pull: Func,
    finish: Func,
) -> Result<Func> {
    let f = builder::declare(module, "web.fill-view", &[I32; 5], &[I32; 3]);
    let mut b = Builder::new(module, f, memory);
    let stream = b.param(0);
    let reader = b.param(1);
    let view = b.param(2);
    let minimum = b.param(3);
    let data = b.load(view, 0, I32);
    let offset = b.load(view, 12, I32);
    let capacity = b.load(view, 4, I32);
    let state = b.param(4);
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let queued = b.load(stream, PENDING_CHUNK, I32);
    let check_queue = b.body.add_block();
    let begin = b.body.add_block();
    b.branch(queued, check_queue, begin);
    b.block = check_queue;
    let available = b.load(queued, 4, I32);
    let short = b.op(O::I32LtU, &[available, minimum], I32);
    let eof = b.load(stream, EOF, I32);
    let closing = b.op(O::I32Eq, &[eof, one], I32);
    let invalid = b.op(O::I32And, &[short, closing], I32);
    let reject = b.body.add_block();
    b.branch(invalid, reject, begin);
    b.block = reject;
    b.call(finish, &[stream], &[I32]);
    let error = b.integer(12);
    b.store(stream, STATE, two, I32);
    b.store(stream, ERROR, error, I32);
    b.store(stream, PENDING_CHUNK, zero, I32);
    b.ret(&[error, zero, one]);
    b.block = begin;
    let next = b.body.add_block();
    let written = b.body.add_blockparam(next, I32);
    let finish = b.body.add_block();
    let error = b.body.add_blockparam(finish, I32);
    let total = b.body.add_blockparam(finish, I32);
    let ended = b.body.add_blockparam(finish, I32);
    b.jump(next, &[zero]);
    b.block = next;
    let pointer = b.op(O::I32Add, &[data, written], I32);
    let position = b.op(O::I32Add, &[offset, written], I32);
    let remaining = b.op(O::I32Sub, &[capacity, written], I32);
    b.store(view, 0, pointer, I32);
    b.store(view, 12, position, I32);
    b.store(view, 4, remaining, I32);
    let values = b.call(pull, &[stream, view], &[I32; 3]);
    let chunk = b.body.add_block();
    let end = b.body.add_block();
    b.branch(values[1], chunk, end);
    b.block = end;
    b.jump(finish, &[values[0], written, values[2]]);
    b.block = chunk;
    let length = b.load(view, 4, I32);
    let count = b.op(O::I32Add, &[written, length], I32);
    let enough = b.op(O::I32GeU, &[count, minimum], I32);
    let attached = b.load(reader, 4, I32);
    let released = b.op(O::I32Eqz, &[attached], I32);
    let stop = b.op(O::I32Or, &[enough, released], I32);
    let success = b.body.add_block();
    // The continuation carries the accumulated count.
    b.body.set_terminator(
        b.block,
        waffle::Terminator::CondBr {
            cond: stop,
            if_true: waffle::BlockTarget {
                block: success,
                args: vec![],
            },
            if_false: waffle::BlockTarget {
                block: next,
                args: vec![count],
            },
        },
    );
    b.block = success;
    b.jump(finish, &[zero, count, zero]);
    b.block = finish;
    b.store(view, 0, data, I32);
    b.store(view, 12, offset, I32);
    b.store(view, 4, total, I32);
    let eof = b.load(stream, EOF, I32);
    let cancelled = b.op(O::I32Eq, &[eof, two], I32);
    let started_open = b.op(O::I32Eqz, &[state], I32);
    let cancelled = b.op(O::I32And, &[cancelled, started_open], I32);
    let no_value = b.op(O::I32Or, &[cancelled, error], I32);
    let result = b.op(O::Select, &[zero, view, no_value], I32);
    let ended = b.op(O::Select, &[one, ended, no_value], I32);
    b.ret(&[error, result, ended]);
    b.finish(module, f)?;
    Ok(f)
}

/// A default chunk exposes only received bytes; transferred buffers keep their full capacity.
pub(in crate::waffle_backend::streams) fn received(
    b: &mut Builder,
    r: &Runtime<'_>,
    provided: Value,
    view: Value,
    length: Value,
) {
    b.store(view, 4, length, I32);
    if !r.readers.has_default() {
        return;
    }
    if r.readers.has_byob() {
        let resize = b.body.add_block();
        let done = b.body.add_block();
        b.branch(provided, done, resize);
        b.block = resize;
        let buffer = b.load(view, 8, I32);
        b.store(buffer, 4, length, I32);
        b.jump(done, &[]);
        b.block = done;
    } else {
        let buffer = b.load(view, 8, I32);
        b.store(buffer, 4, length, I32);
    }
}
