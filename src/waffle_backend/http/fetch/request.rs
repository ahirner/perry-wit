//! Request validation and independently scheduled, retained upload buffers.

use super::*;

pub(super) fn invalid(b: &mut Builder, t: &Transport<'_>, frame: Value, condition: Value) {
    let bad = b.body.add_block();
    let next = b.body.add_block();
    b.branch(condition, bad, next);
    b.block = bad;
    b.call(t.allocator.frame_drop, &[frame], &[]);
    let one = b.integer(1);
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = next;
}

pub(super) fn method(b: &mut Builder, t: &Transport<'_>, frame: Value) -> Result<[Value; 3]> {
    let zero = b.integer(0);
    let explicit = b.body.add_block();
    let default = b.body.add_block();
    let checked = b.body.add_block();
    let tag = b.body.add_blockparam(checked, I32);
    let result_data = b.body.add_blockparam(checked, I32);
    let result_length = b.body.add_blockparam(checked, I32);
    b.branch(b.param(1), explicit, default);
    b.block = default;
    b.jump(checked, &[zero, zero, zero]);
    b.block = explicit;
    let data = b.load(b.param(1), 0, I32);
    let length = b.load(b.param(1), 4, I32);
    let method = b.call(t.native["fetch_method"], &[data, length], &[I32])[0];
    let error = b.integer(u32::MAX);
    let error = b.op(O::I32Eq, &[method, error], I32);
    invalid(b, t, frame, error);
    b.jump(checked, &[method, data, length]);
    b.block = checked;
    let one = b.integer(1);
    let get_or_head = b.op(O::I32LeU, &[tag, one], I32);
    let body = b.op(O::I32Ne, &[b.param(4), zero], I32);
    let prohibited = b.op(O::I32And, &[get_or_head, body], I32);
    invalid(b, t, frame, prohibited);
    Ok([tag, result_data, result_length])
}

pub(super) fn fields(
    b: &mut Builder,
    t: &Transport<'_>,
    frame: Value,
    scratch: Value,
    response: Value,
) -> Result<()> {
    let zero = b.integer(0);
    let one = b.integer(1);
    let stride = b.integer(16);
    let headers = b.load(frame, 32, I32);
    let supplied = b.body.add_block();
    let empty = b.body.add_block();
    let scan = b.body.add_block();
    let data = b.body.add_blockparam(scan, I32);
    let count = b.body.add_blockparam(scan, I32);
    b.branch(headers, supplied, empty);
    b.block = empty;
    b.jump(scan, &[zero, zero]);
    b.block = supplied;
    let fields = b.load(headers, 4, I32);
    let length = b.load(headers, 8, I32);
    b.jump(scan, &[fields, length]);
    b.block = scan;
    let search = b.body.add_block();
    let index = b.body.add_blockparam(search, I32);
    let has_type = b.body.add_blockparam(search, I32);
    b.jump(search, &[zero, zero]);
    b.block = search;
    let more = b.op(O::I32LtU, &[index, count], I32);
    let item = b.body.add_block();
    let counted = b.body.add_block();
    b.branch(more, item, counted);
    b.block = item;
    let offset = b.op(O::I32Mul, &[index, stride], I32);
    let field = b.op(O::I32Add, &[data, offset], I32);
    let name = b.load(field, 0, I32);
    let length = b.load(field, 4, I32);
    let classification = b.call(t.native["fetch_header"], &[name, length], &[I32])[0];
    let found = b.op(O::I32Or, &[has_type, classification], I32);
    let next = b.op(O::I32Add, &[index, one], I32);
    b.jump(search, &[next, found]);
    b.block = counted;
    let text_body = b.op(O::I32Eq, &[b.param(4), one], I32);
    let no_type = b.op(O::I32Eqz, &[has_type], I32);
    let default_type = b.op(O::I32And, &[text_body, no_type], I32);
    let max = b.integer(u32::MAX / 16);
    let fits = b.op(O::I32LtU, &[count, max], I32);
    b.require(fits);
    let total = b.op(O::I32Add, &[count, default_type], I32);
    let size = b.op(O::I32Mul, &[total, stride], I32);
    let alignment = b.integer(4);
    let list = b.call(t.allocator.realloc, &[zero, zero, alignment, size], &[I32])[0];
    b.store(frame, 40, list, I32);
    let length = b.op(O::I32Mul, &[count, stride], I32);
    b.effect(
        O::MemoryCopy {
            dst_mem: b.memory(0).memory,
            src_mem: b.memory(0).memory,
        },
        &[list, data, length],
    );
    let append = b.body.add_block();
    let construct = b.body.add_block();
    b.branch(default_type, append, construct);
    b.block = append;
    let target = b.op(O::I32Add, &[list, length], I32);
    for (descriptor, at) in t.content_type.into_iter().zip([0, 8]) {
        let source = b.integer(descriptor);
        for part in [0, 4] {
            let value = b.load(source, part, I32);
            b.store(target, at + part, value, I32);
        }
    }
    b.jump(construct, &[]);
    b.block = construct;
    b.store(response, 4, list, I32);
    b.store(response, 8, total, I32);
    b.call(t.native["fields"], &[list, total, scratch], &[]);
    let failed = byte(b, scratch, 0);
    invalid(b, t, frame, failed);
    Ok(())
}

pub(super) fn start_upload(
    b: &mut Builder,
    t: &Transport<'_>,
    response: Value,
    writer: Value,
    has_body: Value,
) {
    let upload = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(has_body, upload, ready);
    b.block = upload;
    let body = b.param(3);
    let kind = b.integer(3);
    let record = b.call(t.promises.new, &[kind], &[I32])[0];
    b.store(response, 60, record, I32);
    b.call(
        t.promises.starts[&crate::waffle_backend::promises::TaskTarget::FetchUpload],
        &[record, writer, body],
        &[I32],
    );
    b.jump(ready, &[]);
    b.block = ready;
}

pub(super) fn emit_upload(
    module: &mut Module<'static>,
    memory: Memory,
    native: &BTreeMap<String, Func>,
) -> Result<Func> {
    let write = streams::transfer::write(module, memory, native["write"])?;
    let function = builder::declare(module, "fetch.upload", &[I32; 2], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let writer = b.param(0);
    let body = b.param(1);
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
