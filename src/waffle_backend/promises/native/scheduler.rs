//! FIFO source reactions share one execution token; native I/O remains concurrent.
//! Thread startup queues the caller until the child's first native suspension or await.
//! The shared context marks that eager prefix; once the caller resumes, further
//! awaits join the reaction queue instead of resuming the caller a second time.

use super::*;
use waffle::Value;

pub(super) const READY_HEAD: u32 = 80;
const READY_TAIL: u32 = 84;
pub(super) const ACTIVE: u32 = 88;

pub(super) fn append(b: &mut Builder, head: Value, tail: Value) {
    use Type::I32;
    let nonempty = b.body.add_block();
    let done = b.body.add_block();
    b.branch(head, nonempty, done);
    b.block = nonempty;
    let head_address = b.integer(READY_HEAD);
    let tail_address = b.integer(READY_TAIL);
    let previous = b.load(tail_address, 0, I32);
    let append = b.body.add_block();
    let first = b.body.add_block();
    let linked = b.body.add_block();
    b.branch(previous, append, first);
    b.block = append;
    b.store(previous, 4, head, I32);
    b.jump(linked, &[]);
    b.block = first;
    b.store(head_address, 0, head, I32);
    b.jump(linked, &[]);
    b.block = linked;
    b.store(tail_address, 0, tail, I32);
    b.jump(done, &[]);
    b.block = done;
}

fn waiter(b: &mut Builder, registry: &ModuleRegistry, thread: Value) -> Value {
    use Type::I32;
    let node = allocate(b, registry, 8);
    let four = b.integer(4);
    let header = b.op(Operator::I32Sub, &[node, four], I32);
    let header = b.load(header, 0, I32);
    let kind = b.integer(5);
    b.store(header, 16, kind, I32);
    b.store(node, 0, thread, I32);
    let zero = b.integer(0);
    b.store(node, 4, zero, I32);
    node
}

pub(super) fn emit(module: &mut Module<'static>, registry: &ModuleRegistry) -> Result<()> {
    use Type::I32;
    let native = &registry.promises.as_ref().unwrap().native;
    let memory = registry.memory;
    let mut b = Builder::new(module, native.enqueue, memory);
    let thread = b.param(0);
    let node = waiter(&mut b, registry, thread);
    append(&mut b, node, node);
    b.ret(&[]);
    b.finish(module, native.enqueue)?;

    let mut b = Builder::new(module, native.observe, memory);
    let record = b.param(0);
    let thread = b.param(1);
    let node = waiter(&mut b, registry, thread);
    let status = b.load(record, 4, I32);
    let two = b.integer(2);
    let pending = b.op(Operator::I32Eq, &[status, two], I32);
    let wait = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(pending, wait, ready);
    b.block = ready;
    append(&mut b, node, node);
    b.ret(&[]);
    b.block = wait;
    let tail = b.load(record, 24, I32);
    let append = b.body.add_block();
    let first = b.body.add_block();
    let done = b.body.add_block();
    b.branch(tail, append, first);
    b.block = append;
    b.store(tail, 4, node, I32);
    b.jump(done, &[]);
    b.block = first;
    b.store(record, 20, node, I32);
    b.jump(done, &[]);
    b.block = done;
    b.store(record, 24, node, I32);
    b.ret(&[]);
    b.finish(module, native.observe)?;

    let mut b = Builder::new(module, native.dispatch, memory);
    let active_address = b.integer(ACTIVE);
    let active = b.load(active_address, 0, I32);
    let idle = b.body.add_block();
    let done = b.body.add_block();
    b.branch(active, done, idle);
    b.block = idle;
    let head_address = b.integer(READY_HEAD);
    let tail_address = b.integer(READY_TAIL);
    let head = b.load(head_address, 0, I32);
    let pop = b.body.add_block();
    b.branch(head, pop, done);
    b.block = pop;
    let next = b.load(head, 4, I32);
    b.store(head_address, 0, next, I32);
    let last = b.body.add_block();
    let resume = b.body.add_block();
    b.branch(next, resume, last);
    b.block = last;
    let zero = b.integer(0);
    b.store(tail_address, 0, zero, I32);
    b.jump(resume, &[]);
    b.block = resume;
    let one = b.integer(1);
    b.store(active_address, 0, one, I32);
    let thread = b.load(head, 0, I32);
    let current = b.call(native.index, &[], &[I32])[0];
    let same = b.op(Operator::I32Eq, &[thread, current], I32);
    let continue_current = b.body.add_block();
    let resume_other = b.body.add_block();
    b.branch(same, continue_current, resume_other);
    b.block = continue_current;
    b.ret(&[one]);
    b.block = resume_other;
    b.call(native.resume, &[thread], &[]);
    b.jump(done, &[]);
    b.block = done;
    let zero = b.integer(0);
    b.ret(&[zero]);
    b.finish(module, native.dispatch)?;

    for (function, suspend) in [(native.pause, true), (native.complete, false)] {
        let mut b = Builder::new(module, function, memory);
        let context = b.call(native.context_get, &[], &[I32])[0];
        let inspect = b.body.add_block();
        let handoff = b.body.add_block();
        let release = b.body.add_block();
        b.branch(context, inspect, release);
        b.block = inspect;
        let eager = b.load(context, 4, I32);
        b.branch(eager, handoff, release);
        b.block = handoff;
        let zero = b.integer(0);
        b.store(context, 4, zero, I32);
        if suspend {
            b.call(native.suspend, &[], &[I32]);
        }
        b.ret(&[]);
        b.block = release;
        let active = b.integer(ACTIVE);
        let zero = b.integer(0);
        b.store(active, 0, zero, I32);
        let same = b.call(native.dispatch, &[], &[I32])[0];
        if suspend {
            let wait = b.body.add_block();
            let resumed = b.body.add_block();
            b.branch(same, resumed, wait);
            b.block = wait;
            b.call(native.suspend, &[], &[I32]);
            b.jump(resumed, &[]);
            b.block = resumed;
        }
        b.ret(&[]);
        b.finish(module, function)?;
    }
    Ok(())
}
