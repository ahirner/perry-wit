//! Promise observer lists retain suspended threads until settlement.

use super::*;
use waffle::Value;

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
    b.call(native.scheduler.wake_source, &[thread], &[]);
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

    Ok(())
}
