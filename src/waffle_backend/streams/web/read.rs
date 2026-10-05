use super::*;

pub(super) fn emit_finish(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
    f: Func,
) -> Result<()> {
    let mut b = Builder::new(module, f, memory);
    let stream = b.param(0);
    let state = b.load(stream, STATE, I32);
    let one = b.integer(CLOSED);
    let two = b.integer(ERRORED);
    let closed = b.op(O::I32Eq, &[state, one], I32);
    let errored = b.op(O::I32Eq, &[state, two], I32);
    let terminal = b.op(O::I32Or, &[closed, errored], I32);
    let done = b.body.add_block();
    let close = b.body.add_block();
    b.branch(terminal, done, close);
    b.block = done;
    let error = b.load(stream, ERROR, I32);
    b.ret(&[error]);
    b.block = close;
    let native = is_native(&mut b, stream);
    let release = b.body.add_block();
    let buffered = b.body.add_block();
    let finish = b.body.add_block();
    let error = b.body.add_blockparam(finish, I32);
    b.branch(native, release, buffered);
    b.block = buffered;
    let zero = b.integer(0);
    b.jump(finish, &[zero]);
    b.block = release;
    if let Some(native) = r.native {
        let body = b.load(stream, BODY, I32);
        let frame = b.load(body, 56, I32);
        let error = b.call(native.release, &[body], &[I32])[0];
        b.call(r.allocator.frame_drop, &[frame], &[]);
        b.jump(finish, &[error]);
    } else {
        b.body
            .set_terminator(b.block, waffle::Terminator::Unreachable);
    }
    b.block = finish;
    let state = b.op(O::Select, &[two, one, error], I32);
    b.store(stream, STATE, state, I32);
    b.store(stream, ERROR, error, I32);
    b.ret(&[error]);
    b.finish(module, f)
}
pub(super) fn emit_pull(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
    f: Func,
    finish: Func,
) -> Result<()> {
    let mut b = Builder::new(module, f, memory);
    let stream = b.param(0);
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let used = used_address(&mut b, stream);
    b.store(used, 0, one, I32);
    let state = b.load(stream, STATE, I32);
    let closed = b.op(O::I32Eq, &[state, one], I32);
    let done = b.body.add_block();
    let check_error = b.body.add_block();
    b.branch(closed, done, check_error);
    b.block = done;
    b.ret(&[zero, zero, one]);
    b.block = check_error;
    let errored = b.op(O::I32Eq, &[state, two], I32);
    let error = b.body.add_block();
    let check_end = b.body.add_block();
    b.branch(errored, error, check_end);
    b.block = error;
    let reason = b.load(stream, ERROR, I32);
    b.ret(&[reason, zero, one]);
    b.block = check_end;
    let cancelling = b.integer(CANCELLING);
    let cancelling = b.op(O::I32Eq, &[state, cancelling], I32);
    let complete = b.body.add_block();
    let check_chunk = b.body.add_block();
    b.branch(cancelling, complete, check_chunk);
    b.block = check_chunk;
    let pending = b.load(stream, PENDING_CHUNK, I32);
    let queued = b.body.add_block();
    let check_eof = b.body.add_block();
    b.branch(pending, queued, check_eof);
    b.block = queued;
    b.store(stream, PENDING_CHUNK, zero, I32);
    b.ret(&[zero, pending, zero]);
    b.block = check_eof;
    let eof = b.load(stream, EOF, I32);
    let source = b.body.add_block();
    b.branch(eof, complete, source);
    b.block = complete;
    let reason = b.call(finish, &[stream], &[I32])[0];
    b.ret(&[reason, zero, one]);
    b.block = source;
    let native = is_native(&mut b, stream);
    let read = b.body.add_block();
    let buffered = b.body.add_block();
    b.branch(native, read, buffered);
    b.block = buffered;
    let body = b.load(stream, BODY, I32);
    let bytes = b.load(body, 12, I32);
    let copy = b.call(r.bytes.copy, &[bytes], &[I32])[0];
    b.store(stream, EOF, one, I32);
    b.ret(&[zero, copy, zero]);
    b.block = read;
    if let Some(native) = r.native {
        let body = b.load(stream, BODY, I32);
        let signal = b.load(body, crate::waffle_backend::http::response::SIGNAL, I32);
        b.call(native.operations.bind_signal, &[signal], &[]);
        let aborted = b.call(native.operations.aborted, &[], &[I32])[0];
        let abort = b.body.add_block();
        let allocate = b.body.add_block();
        b.branch(aborted, abort, allocate);
        b.block = abort;
        b.call(finish, &[stream], &[I32]);
        let code = b.integer(20);
        b.store(stream, ERROR, code, I32);
        b.store(stream, STATE, two, I32);
        b.ret(&[code, zero, one]);
        b.block = allocate;
        let count = b.integer(2);
        let frame = b.call(r.allocator.frame_new, &[count], &[I32])[0];
        b.store(frame, 12, stream, I32);
        let data = b.allocate(r.allocator.realloc, 16384, 1);
        b.store(frame, 16, data, I32);
        let reader = b.load(body, 24, I32);
        let capacity = b.integer(16384);
        let transfer = b.body.add_block();
        b.jump(transfer, &[]);
        b.block = transfer;
        let status = b.call(native.read, &[reader, data, capacity], &[I32])[0];
        let controller = b.integer(1);
        let owner = b.call(
            native.operations.register_transfer.unwrap(),
            &[reader, status, controller],
            &[I32],
        )[0];
        b.store(stream, TRANSFER, owner, I32);
        let packed = b.call(native.operations.wait, &[owner], &[I32])[0];
        b.store(stream, TRANSFER, zero, I32);
        let mask = b.integer(15);
        let shift = b.integer(4);
        let status = b.op(O::I32And, &[packed, mask], I32);
        let length = b.op(O::I32ShrU, &[packed, shift], I32);
        let valid = b.op(O::I32LeU, &[status, two], I32);
        b.require(valid);
        let valid = b.op(O::I32LeU, &[length, capacity], I32);
        b.require(valid);
        let state = b.load(stream, STATE, I32);
        let cancelling = b.integer(CANCELLING);
        let cancelling = b.op(O::I32Eq, &[state, cancelling], I32);
        let aborted = b.call(native.operations.aborted, &[], &[I32])[0];
        let interrupted = b.op(O::I32Eq, &[status, two], I32);
        let interrupted = b.op(O::I32Or, &[interrupted, aborted], I32);
        let stopped = b.op(O::I32Or, &[cancelling, interrupted], I32);
        let cancel = b.body.add_block();
        let received = b.body.add_block();
        b.branch(stopped, cancel, received);
        b.block = cancel;
        let reason = b.call(finish, &[stream], &[I32])[0];
        let abortcode = b.integer(20);
        let failed = b.op(O::Select, &[abortcode, reason, interrupted], I32);
        let reason = b.op(O::Select, &[reason, failed, cancelling], I32);
        let finalstate = b.op(O::Select, &[two, one, reason], I32);
        b.store(stream, STATE, finalstate, I32);
        b.store(stream, ERROR, reason, I32);
        b.call(r.allocator.frame_drop, &[frame], &[]);
        b.ret(&[reason, zero, one]);
        b.block = received;
        let closed = b.op(O::I32Eq, &[status, one], I32);
        b.store(stream, EOF, closed, I32);
        let bytes = b.body.add_block();
        let empty = b.body.add_block();
        b.branch(length, bytes, empty);
        b.block = empty;
        let end = b.body.add_block();
        b.branch(closed, end, transfer);
        b.block = end;
        let reason = b.call(finish, &[stream], &[I32])[0];
        b.call(r.allocator.frame_drop, &[frame], &[]);
        b.ret(&[reason, zero, one]);
        b.block = bytes;
        let view = b.call(r.bytes.lift_canonical, &[data, length], &[I32])[0];
        b.call(r.allocator.frame_drop, &[frame], &[]);
        b.ret(&[zero, view, zero]);
    } else {
        b.body
            .set_terminator(b.block, waffle::Terminator::Unreachable);
    }
    b.finish(module, f)
}
pub(super) fn emit_result(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
    f: Func,
) -> Result<()> {
    let mut b = Builder::new(module, f, memory);
    let bytes = b.param(0);
    let done = b.param(1);
    let count = b.integer(2);
    let frame = b.call(r.allocator.frame_new, &[count], &[I32])[0];
    b.store(frame, 12, bytes, I32);
    let result = b.call(r.objects.new, &[], &[I32])[0];
    b.store(frame, 16, result, I32);
    let key = b.integer(r.pool.get("done").unwrap());
    let tag = b.integer(2);
    let payload = b.op(O::F64ConvertI32U, &[done], F64);
    b.call(r.objects.set, &[result, key, tag, payload], &[I32, F64]);
    let key = b.integer(r.pool.get("value").unwrap());
    let zero = b.integer(0);
    let tag = b.integer(5);
    let tag = b.op(O::Select, &[zero, tag, done], I32);
    let payload = b.op(O::F64ConvertI32U, &[bytes], F64);
    b.call(r.objects.set, &[result, key, tag, payload], &[I32, F64]);
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[result]);
    b.finish(module, f)
}
/// The public read can reject on release while its transfer still owns storage.
pub(super) fn emit_read(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
    f: Func,
    pull: Func,
    result: Func,
) -> Result<()> {
    let mut b = Builder::new(module, f, memory);
    let reader = b.param(0);
    let stream = b.load(reader, 4, I32);
    let detached = b.op(O::I32Eqz, &[stream], I32);
    reject(&mut b, detached, 12);
    let Some(promises) = r.promises else {
        b.body
            .set_terminator(b.block, waffle::Terminator::Unreachable);
        return b.finish(module, f);
    };
    let three = b.integer(3);
    let frame = b.call(r.allocator.frame_new, &[three], &[I32])[0];
    b.store(frame, 12, stream, I32);
    let node = tagged_allocation(&mut b, r.allocator, 16, 18);
    b.store(frame, 16, node, I32);
    let head = b.load(reader, 0, I32);
    b.store(node, 4, head, I32);
    b.store(node, 8, b.param(2), I32);
    let link = b.body.add_block();
    let linked = b.body.add_block();
    b.branch(head, link, linked);
    b.block = link;
    b.store(head, 0, node, I32);
    b.jump(linked, &[]);
    b.block = linked;
    b.store(reader, 0, node, I32);
    let completion = b.call(promises.new, &[three], &[I32])[0];
    b.store(node, 12, completion, I32);
    let previous = b.load(stream, LAST_READ, I32);
    b.store(frame, 20, previous, I32);
    b.store(stream, LAST_READ, completion, I32);
    let finish = b.body.add_block();
    let outcome_tag = b.body.add_blockparam(finish, I32);
    let outcome_payload = b.body.add_blockparam(finish, F64);
    let released = b.body.add_block();
    let wait = b.body.add_block();
    let read = b.body.add_block();
    b.branch(previous, wait, read);
    b.block = wait;
    b.call(promises.await_native, &[previous], &[I32, F64]);
    b.jump(read, &[]);
    b.block = read;
    let attached = b.load(reader, 4, I32);
    let pull_block = b.body.add_block();
    b.branch(attached, pull_block, released);
    b.block = pull_block;
    let values = b.call(pull, &[stream], &[I32; 3]);
    let attached = b.load(reader, 4, I32);
    let requeue = b.body.add_block();
    let observed = b.body.add_block();
    b.branch(attached, observed, requeue);
    b.block = requeue;
    b.store(stream, PENDING_CHUNK, values[1], I32);
    b.jump(released, &[]);
    b.block = released;
    let one = b.integer(1);
    let error = b.number(12.0);
    b.jump(finish, &[one, error]);
    b.block = observed;
    let fail = b.body.add_block();
    let success = b.body.add_block();
    b.branch(values[0], fail, success);
    b.block = fail;
    let error = b.op(O::F64ConvertI32U, &[values[0]], F64);
    let one = b.integer(1);
    b.jump(finish, &[one, error]);
    b.block = success;
    let value = b.call(result, &[values[1], values[2]], &[I32])[0];
    let payload = b.op(O::F64ConvertI32U, &[value], F64);
    let zero = b.integer(0);
    b.jump(finish, &[zero, payload]);
    b.block = finish;
    let previous = b.load(node, 0, I32);
    let next = b.load(node, 4, I32);
    let first = b.body.add_block();
    let middle = b.body.add_block();
    let unlinked = b.body.add_block();
    b.branch(previous, middle, first);
    b.block = middle;
    b.store(previous, 4, next, I32);
    b.jump(unlinked, &[]);
    b.block = first;
    b.store(reader, 0, next, I32);
    b.jump(unlinked, &[]);
    b.block = unlinked;
    let following = b.body.add_block();
    let done = b.body.add_block();
    b.branch(next, following, done);
    b.block = following;
    b.store(next, 0, previous, I32);
    b.jump(done, &[]);
    b.block = done;
    let zero = b.integer(0);
    let empty = b.number(0.0);
    b.call(promises.native.settle, &[completion, zero, empty], &[]);
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[outcome_tag, outcome_payload]);
    b.finish(module, f)
}
pub(super) fn emit_cancel(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
    f: Func,
    finish: Func,
) -> Result<()> {
    let mut b = Builder::new(module, f, memory);
    let receiver = b.param(0);
    let reader = b.param(1);
    let by_reader = b.body.add_block();
    let by_stream = b.body.add_block();
    let ready = b.body.add_block();
    let stream = b.body.add_blockparam(ready, I32);
    b.branch(reader, by_reader, by_stream);
    b.block = by_reader;
    let value = b.load(receiver, 4, I32);
    let detached = b.op(O::I32Eqz, &[value], I32);
    reject(&mut b, detached, 12);
    b.jump(ready, &[value]);
    b.block = by_stream;
    let locked = b.load(receiver, LOCK, I32);
    reject(&mut b, locked, 12);
    b.jump(ready, &[receiver]);
    b.block = ready;
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let frame = b.call(r.allocator.frame_new, &[two], &[I32])[0];
    b.store(frame, 12, stream, I32);
    let used = used_address(&mut b, stream);
    b.store(used, 0, one, I32);
    let state = b.load(stream, STATE, I32);
    let open = b.op(O::I32Eqz, &[state], I32);
    let cancel = b.body.add_block();
    let wait = b.body.add_block();
    b.branch(open, cancel, wait);
    b.block = cancel;
    let cancelling = b.integer(CANCELLING);
    b.store(stream, STATE, cancelling, I32);
    if let Some(native) = r.native {
        let owner = b.load(stream, TRANSFER, I32);
        let pending = b.body.add_block();
        let done = b.body.add_block();
        b.branch(owner, pending, done);
        b.block = pending;
        b.call(native.operations.cancel, &[owner], &[]);
        b.jump(done, &[]);
        b.block = done;
    }
    b.jump(wait, &[]);
    b.block = wait;
    let previous = b.load(stream, LAST_READ, I32);
    b.store(frame, 16, previous, I32);
    b.store(stream, LAST_READ, b.param(2), I32);
    let pending = b.body.add_block();
    let finish_block = b.body.add_block();
    b.branch(previous, pending, finish_block);
    b.block = pending;
    if let Some(promises) = r.promises {
        b.call(promises.await_native, &[previous], &[I32, F64]);
    }
    b.jump(finish_block, &[]);
    b.block = finish_block;
    let error = b.call(finish, &[stream], &[I32])[0];
    let thrown = b.op(O::Select, &[one, zero, error], I32);
    let payload = b.op(O::F64ConvertI32U, &[error], F64);
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[thrown, payload]);
    b.finish(module, f)
}
