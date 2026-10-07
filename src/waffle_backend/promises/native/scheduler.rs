//! P3 schedules runnable continuations; records retain only pending observers.
//! The immediate caller resumes at a child's first suspension, preserving eager starts.
//! Settled awaits need no host yield when neither another source continuation nor
//! native work can advance; pending native work still gets its polling opportunity.

use super::*;
use waffle::Value;

pub(crate) const RUNNABLE: u32 = 88;

pub(super) fn update_runnable(b: &mut Builder, started: bool) {
    use Type::I32;
    let address = b.integer(RUNNABLE);
    let count = b.load(address, 0, I32);
    let one = b.integer(1);
    if !started {
        b.require(count);
    }
    let count = b.op(
        if started {
            Operator::I32Add
        } else {
            Operator::I32Sub
        },
        &[count, one],
        I32,
    );
    b.store(address, 0, count, I32);
}

pub(super) fn waiter(b: &mut Builder, registry: &ModuleRegistry, thread: Value) -> Value {
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
    let runtime = registry.promises.as_ref().unwrap();
    let native = &runtime.native;
    let memory = registry.memory;
    let mut b = Builder::new(module, native.schedule, memory);
    update_runnable(&mut b, true);
    b.call(native.resume, &[b.param(0)], &[]);
    b.ret(&[]);
    b.finish(module, native.schedule)?;

    let mut b = Builder::new(module, native.observe, memory);
    let record = b.param(0);
    let thread = b.param(1);
    let status = b.load(record, 4, I32);
    let two = b.integer(2);
    let pending = b.op(Operator::I32Eq, &[status, two], I32);
    let wait = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(pending, wait, ready);
    b.block = ready;
    b.call(native.schedule, &[thread], &[]);
    b.ret(&[]);
    b.block = wait;
    let node = waiter(&mut b, registry, thread);
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

    enum Continuation {
        Suspend,
        Complete,
        Yield,
    }
    for (function, continuation) in [
        (native.pause, Continuation::Suspend),
        (native.complete, Continuation::Complete),
        (runtime.yield_thread, Continuation::Yield),
    ] {
        let mut b = Builder::new(module, function, memory);
        if !matches!(continuation, Continuation::Yield) {
            update_runnable(&mut b, false);
        }
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
        let one = b.integer(1);
        let parent = b.op(Operator::I32Sub, &[eager, one], I32);
        b.call(
            if matches!(continuation, Continuation::Suspend) {
                native.suspend_then_promote
            } else {
                native.yield_then_promote
            },
            &[parent],
            &[I32],
        );
        b.ret(&[]);
        b.block = release;
        if matches!(continuation, Continuation::Yield) {
            let address = b.integer(RUNNABLE);
            let runnable = b.load(address, 0, I32);
            let one = b.integer(1);
            let alone = b.op(Operator::I32Eq, &[runnable, one], I32);
            let address = b.integer(crate::waffle_backend::runtime::callbacks::NATIVE_WORKERS);
            let workers = b.load(address, 0, I32);
            let pending = if let Some(operations) = registry.operations {
                let pending = b.call(operations.pending, &[], &[I32])[0];
                b.op(Operator::I32Or, &[workers, pending], I32)
            } else {
                workers
            };
            let idle = b.op(Operator::I32Eqz, &[pending], I32);
            let uncontended = b.op(Operator::I32And, &[alone, idle], I32);
            let ready = b.body.add_block();
            let yield_host = b.body.add_block();
            b.branch(uncontended, ready, yield_host);
            b.block = ready;
            b.ret(&[]);
            b.block = yield_host;
        }
        if !matches!(continuation, Continuation::Complete) {
            b.call(
                if matches!(continuation, Continuation::Suspend) {
                    native.suspend
                } else {
                    native.yield_now
                },
                &[],
                &[I32],
            );
        }
        b.ret(&[]);
        b.finish(module, function)?;
    }
    Ok(())
}
