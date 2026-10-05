//! Independently scheduled uploads retain their immutable request buffers.

use super::*;

pub(super) fn start(
    b: &mut Builder,
    t: &Transport<'_>,
    response: Value,
    writer: Value,
    has_body: Value,
    body: Value,
) {
    let upload = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(has_body, upload, ready);
    b.block = upload;
    let kind = b.integer(3);
    let record = b.call(t.promises.new, &[kind], &[I32])[0];
    b.store(response, 60, record, I32);
    let signal = b.load(response, super::super::response::SIGNAL, I32);
    b.call(
        t.promises.starts[&crate::waffle_backend::promises::TaskTarget::FetchUpload],
        &[record, writer, body, signal],
        &[I32],
    );
    b.jump(ready, &[]);
    b.block = ready;
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    native: &BTreeMap<String, Func>,
    operations: Option<crate::waffle_backend::runtime::operations::Operations>,
) -> Result<Func> {
    let write = streams::transfer::write(module, memory, native["write"])?;
    let function = builder::declare(module, "fetch.upload", &[I32; 3], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let writer = b.param(0);
    let body = b.param(1);
    if let Some(operations) = operations {
        b.call(operations.bind_signal, &[b.param(2)], &[]);
    }
    let data = b.load(body, 0, I32);
    let length = b.load(body, 4, I32);
    let written = b.call(write, &[writer, data, length], &[I32])[0];
    b.call(native["drop-writer"], &[writer], &[]);
    let short = b.op(O::I32Ne, &[written, length], I32);
    let payload = b.op(O::F64ConvertI32U, &[short], F64);
    let zero = b.integer(0);
    b.ret(&[zero, payload]);
    b.finish(module, function)?;
    Ok(function)
}
