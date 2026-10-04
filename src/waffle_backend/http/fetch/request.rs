//! Request validation and independently scheduled, retained upload buffers.

use super::*;

fn invalid(b: &mut Builder, t: &Transport<'_>, frame: Value, condition: Value) {
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
    let supplied = b.body.add_block();
    let empty = b.body.add_block();
    let first = b.body.add_block();
    let head = b.body.add_blockparam(first, I32);
    b.branch(b.param(2), supplied, empty);
    b.block = supplied;
    let entry = b.load(b.param(2), 0, I32);
    b.jump(first, &[entry]);
    b.block = empty;
    b.jump(first, &[zero]);
    b.block = first;
    let count_loop = b.body.add_block();
    let entry = b.body.add_blockparam(count_loop, I32);
    let count = b.body.add_blockparam(count_loop, I32);
    let has_type = b.body.add_blockparam(count_loop, I32);
    let next = b.body.add_block();
    let counted = b.body.add_block();
    b.jump(count_loop, &[head, zero, zero]);
    b.block = count_loop;
    b.branch(entry, next, counted);
    b.block = next;
    let key = b.load(entry, 4, I32);
    let data = b.load(key, 0, I32);
    let length = b.load(key, 4, I32);
    let classification = b.call(t.native["fetch_header"], &[data, length], &[I32])[0];
    let bad = b.integer(u32::MAX);
    let bad = b.op(O::I32Eq, &[classification, bad], I32);
    invalid(b, t, frame, bad);
    let next_type = b.op(O::I32Or, &[has_type, classification], I32);
    let next_entry = b.load(entry, 0, I32);
    let next_count = b.op(O::I32Add, &[count, one], I32);
    b.jump(count_loop, &[next_entry, next_count, next_type]);
    b.block = counted;
    let max = b.integer(u32::MAX / 16);
    let fits = b.op(O::I32LtU, &[count, max], I32);
    b.require(fits);
    let text_body = b.op(O::I32Eq, &[b.param(4), one], I32);
    let no_type = b.op(O::I32Eqz, &[has_type], I32);
    let default_type = b.op(O::I32And, &[text_body, no_type], I32);
    let count = b.op(O::I32Add, &[count, default_type], I32);
    let stride = b.integer(16);
    let length = b.op(O::I32Mul, &[count, stride], I32);
    let alignment = b.integer(4);
    let list = b.call(
        t.allocator.realloc,
        &[zero, zero, alignment, length],
        &[I32],
    )[0];
    b.store(frame, 40, list, I32);
    b.effect(
        O::MemoryFill {
            mem: b.memory(0).memory,
        },
        &[list, zero, length],
    );
    b.store(response, 4, list, I32);
    b.store(response, 8, count, I32);
    let copy = b.body.add_block();
    let entry = b.body.add_blockparam(copy, I32);
    let target = b.body.add_blockparam(copy, I32);
    let item = b.body.add_block();
    let copied = b.body.add_block();
    b.jump(copy, &[head, list]);
    b.block = copy;
    b.branch(entry, item, copied);
    b.block = item;
    let key = b.load(entry, 4, I32);
    let value = b.load(entry, 16, F64);
    let value = b.op(O::I32TruncF64U, &[value], I32);
    for at in [0, 4] {
        let data = b.load(key, at, I32);
        b.store(target, at, data, I32);
    }
    let data = b.load(value, 0, I32);
    let length = b.load(value, 4, I32);
    let capacity = b.op(O::Select, &[length, one, length], I32);
    let normalized = b.call(t.allocator.realloc, &[zero, zero, one, capacity], &[I32])[0];
    b.store(target, 8, normalized, I32);
    let length = b.call(
        t.native["fetch_header_value"],
        &[data, length, normalized, capacity],
        &[I32],
    )[0];
    let error = b.integer(u32::MAX);
    let error = b.op(O::I32Eq, &[length, error], I32);
    invalid(b, t, frame, error);
    b.store(target, 12, length, I32);
    let entry = b.load(entry, 0, I32);
    let next_target = b.op(O::I32Add, &[target, stride], I32);
    b.jump(copy, &[entry, next_target]);
    b.block = copied;
    let append = b.body.add_block();
    let construct = b.body.add_block();
    b.branch(default_type, append, construct);
    b.block = append;
    for (descriptor, at) in t.content_type.into_iter().zip([0, 8]) {
        let source = b.integer(descriptor);
        for part in [0, 4] {
            let value = b.load(source, part, I32);
            b.store(target, at + part, value, I32);
        }
    }
    b.jump(construct, &[]);
    b.block = construct;
    b.call(t.native["fields"], &[list, count, scratch], &[]);
    let failed = byte(b, scratch, 0);
    invalid(b, t, frame, failed);
    Ok(())
}

pub(super) fn start_upload(
    b: &mut Builder,
    t: &Transport<'_>,
    bytes: ByteHelpers,
    response: Value,
    frame: Value,
    writer: Value,
    has_body: Value,
) {
    let upload = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(has_body, upload, ready);
    b.block = upload;
    let two = b.integer(2);
    let byte_body = b.op(O::I32Eq, &[b.param(4), two], I32);
    let snapshot = b.body.add_block();
    let immutable = b.body.add_block();
    let start = b.body.add_block();
    let body = b.body.add_blockparam(start, I32);
    b.branch(byte_body, snapshot, immutable);
    b.block = snapshot;
    let copy = b.call(bytes.copy, &[b.param(3)], &[I32])[0];
    b.store(frame, 36, copy, I32);
    b.jump(start, &[copy]);
    b.block = immutable;
    b.jump(start, &[b.param(3)]);
    b.block = start;
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
