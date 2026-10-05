//! Response owners remain rooted independently of source promise observation.
use super::*;

pub(super) const NEXT: u32 = 80;
const PREVIOUS: u32 = 84;

pub(super) fn drop_reader(b: &mut Builder, t: &Transport<'_>, response: Value, stream: Value) {
    let dropped = b.load(response, super::super::response::READER_DROPPED, I32);
    let drop = b.body.add_block();
    let done = b.body.add_block();
    b.branch(dropped, done, drop);
    b.block = drop;
    let one = b.integer(1);
    b.store(response, super::super::response::READER_DROPPED, one, I32);
    b.call(t.native["drop-reader"], &[stream], &[]);
    b.jump(done, &[]);
    b.block = done;
}

pub(super) fn emit_abort_notify(
    module: &mut Module<'static>,
    memory: Memory,
    t: &Transport<'_>,
) -> Result<()> {
    let function = t.abort.notify;
    let mut b = Builder::new(module, function, memory);
    let three = b.integer(3);
    let frame = b.call(t.allocator.frame_new, &[three], &[I32])[0];
    b.store(frame, 12, b.param(0), I32);
    let address = b.integer(OWNERS);
    let head = b.load(address, 0, I32);
    let next = b.body.add_block();
    let response = b.body.add_blockparam(next, I32);
    let inspect = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[head]);
    b.block = next;
    b.branch(response, inspect, done);
    b.block = inspect;
    b.store(frame, 16, response, I32);
    let following = b.load(response, NEXT, I32);
    b.store(frame, 20, following, I32);
    let signal = b.load(response, super::super::response::SIGNAL, I32);
    let same = b.op(O::I32Eq, &[signal, b.param(0)], I32);
    let used = b.load(response, 20, I32);
    let unused = b.op(O::I32Eqz, &[used], I32);
    let close = b.op(O::I32And, &[same, unused], I32);
    let drop = b.body.add_block();
    let advance = b.body.add_block();
    b.branch(close, drop, advance);
    b.block = drop;
    let stream = b.load(response, 24, I32);
    drop_reader(&mut b, t, response, stream);
    b.jump(advance, &[]);
    b.block = advance;
    b.jump(next, &[following]);
    b.block = done;
    b.call(t.allocator.frame_drop, &[frame], &[]);
    b.ret(&[]);
    b.finish(module, function)
}

pub(super) fn retain(b: &mut Builder, response: Value) {
    let address = b.integer(OWNERS);
    let head = b.load(address, 0, I32);
    b.store(response, NEXT, head, I32);
    b.store(address, 0, response, I32);
    let link = b.body.add_block();
    let done = b.body.add_block();
    b.branch(head, link, done);
    b.block = link;
    b.store(head, PREVIOUS, response, I32);
    b.jump(done, &[]);
    b.block = done;
}

pub(super) fn release(b: &mut Builder, response: Value) {
    let previous = b.load(response, PREVIOUS, I32);
    let next = b.load(response, NEXT, I32);
    let linked = b.body.add_block();
    let first = b.body.add_block();
    let unlinked = b.body.add_block();
    b.branch(previous, linked, first);
    b.block = linked;
    b.store(previous, NEXT, next, I32);
    b.jump(unlinked, &[]);
    b.block = first;
    let address = b.integer(OWNERS);
    b.store(address, 0, next, I32);
    b.jump(unlinked, &[]);
    b.block = unlinked;
    let following = b.body.add_block();
    let done = b.body.add_block();
    b.branch(next, following, done);
    b.block = following;
    b.store(next, PREVIOUS, previous, I32);
    b.jump(done, &[]);
    b.block = done;
    let zero = b.integer(0);
    b.store(response, NEXT, zero, I32);
    b.store(response, PREVIOUS, zero, I32);
}

pub(super) fn emit_cancel_unused(
    module: &mut Module<'static>,
    memory: Memory,
    t: &Transport<'_>,
    release: Func,
) -> Result<Option<Func>> {
    let Some(operations) = t.operations else {
        return Ok(None);
    };
    let function = builder::declare(module, "fetch.cancel-unused", &[], &[]);
    let mut b = Builder::new(module, function, memory);
    use crate::waffle_backend::runtime::callbacks::{Worker, worker_count};
    worker_count(&mut b, true, Worker::Native);
    let cancelled = b.call(operations.cancelled, &[], &[I32])[0];
    let restart = b.body.add_block();
    let done = b.body.add_block();
    b.jump(restart, &[]);
    b.block = restart;
    let address = b.integer(OWNERS);
    let head = b.load(address, 0, I32);
    let next = b.body.add_block();
    let response = b.body.add_blockparam(next, I32);
    b.jump(next, &[head]);
    b.block = next;
    let inspect = b.body.add_block();
    b.branch(response, inspect, done);
    b.block = inspect;
    let used = b.load(response, 20, I32);
    let signal = b.load(response, super::super::response::SIGNAL, I32);
    let aborted = b.call(t.abort.aborted, &[signal], &[I32])[0];
    let aborted = b.op(O::I32Or, &[aborted, cancelled], I32);
    let unused = b.op(O::I32Eqz, &[used], I32);
    let eligible = b.op(O::I32And, &[aborted, unused], I32);
    let skip = b.body.add_block();
    let close = b.body.add_block();
    b.branch(eligible, close, skip);
    b.block = skip;
    let following = b.load(response, NEXT, I32);
    b.jump(next, &[following]);
    b.block = close;
    let one = b.integer(1);
    b.store(response, 20, one, I32);
    let frame = b.load(response, 56, I32);
    b.call(release, &[response], &[I32]);
    b.call(t.allocator.frame_drop, &[frame], &[]);
    b.jump(restart, &[]);
    b.block = done;
    worker_count(&mut b, false, Worker::Native);
    b.ret(&[]);
    b.finish(module, function)?;
    Ok(Some(function))
}
