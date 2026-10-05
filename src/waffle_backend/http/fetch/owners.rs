//! Response owners remain rooted independently of source promise observation.
use super::*;

pub(super) const NEXT: u32 = 80;
const PREVIOUS: u32 = 84;

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
    let cancelled = b.call(operations.cancelled, &[], &[I32])[0];
    let restart = b.body.add_block();
    let done = b.body.add_block();
    b.branch(cancelled, restart, done);
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
    let skip = b.body.add_block();
    let close = b.body.add_block();
    b.branch(used, skip, close);
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
    b.ret(&[]);
    b.finish(module, function)?;
    Ok(Some(function))
}
