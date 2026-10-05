//! Redirect processing keeps one immutable upload snapshot and closes intermediate bodies.
use super::*;

fn propagate(b: &mut Builder, t: &Transport<'_>, frame: Value, result: &[Value]) {
    let failed = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(result[0], failed, ready);
    b.block = failed;
    b.call(t.allocator.frame_drop, &[frame], &[]);
    b.ret(result);
    b.block = ready;
}

pub(super) fn emit_discard(
    module: &mut Module<'static>,
    memory: Memory,
    function: Func,
    t: &Transport<'_>,
    release: Func,
) -> Result<()> {
    let mut b = Builder::new(module, function, memory);
    let response = b.param(0);
    let zero = b.integer(0);
    let one = b.integer(1);
    let null_body = b.load(response, 52, I32);
    let ready = b.body.add_block();
    let close = b.body.add_block();
    b.branch(null_body, ready, close);
    b.block = close;
    b.store(response, 20, one, I32);
    let error = b.call(release, &[response], &[I32])[0];
    let frame = b.load(response, 56, I32);
    b.call(t.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[error], F64);
    let failed = b.op(O::I32Ne, &[error, zero], I32);
    b.ret(&[failed, payload]);
    b.block = ready;
    let payload = b.number(0.0);
    b.ret(&[zero, payload]);
    b.finish(module, function)
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    function: Func,
    send: Func,
    discard: Func,
    t: &Transport<'_>,
    r: &super::super::SourceRuntime<'_>,
) -> Result<()> {
    let mut b = Builder::new(module, function, memory);
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let three = b.integer(3);
    let count = b.integer(9);
    let frame = b.call(t.allocator.frame_new, &[count], &[I32])[0];
    for (offset, index) in [(12, 0), (16, 1), (20, 2), (24, 3), (28, 6)] {
        b.store(frame, offset, b.param(index), I32);
    }
    let supplied = b.body.add_block();
    let default = b.body.add_block();
    let checked = b.body.add_block();
    let mode = b.body.add_blockparam(checked, I32);
    b.branch(b.param(6), supplied, default);
    b.block = default;
    b.jump(checked, &[zero]);
    b.block = supplied;
    let mut valid = zero;
    let mut selected = zero;
    for (index, name) in ["follow", "manual", "error"].into_iter().enumerate() {
        let name = b.integer(r.pool.get(name).unwrap());
        let difference = b.call(r.strings.str_compare, &[b.param(6), name], &[I32])[0];
        let matches = b.op(O::I32Eqz, &[difference], I32);
        valid = b.op(O::I32Or, &[valid, matches], I32);
        let index = b.integer(index as u32);
        selected = b.op(O::Select, &[index, selected, matches], I32);
    }
    let invalid = b.op(O::I32Eqz, &[valid], I32);
    super::request::invalid(&mut b, t, frame, invalid);
    b.jump(checked, &[selected]);
    b.block = checked;
    let headers = b.call(t.headers.new, &[b.param(5), b.param(2)], &[I32, F64]);
    propagate(&mut b, t, frame, &headers);
    let headers = b.op(O::I32TruncF64U, &[headers[1]], I32);
    b.store(frame, 20, headers, I32);
    let byte_body = b.op(O::I32Eq, &[b.param(4), two], I32);
    let copy = b.body.add_block();
    let immutable = b.body.add_block();
    let prepared = b.body.add_block();
    let body = b.body.add_blockparam(prepared, I32);
    b.branch(byte_body, copy, immutable);
    b.block = copy;
    let snapshot = b.call(r.bytes.copy, &[b.param(3)], &[I32])[0];
    b.jump(prepared, &[snapshot]);
    b.block = immutable;
    b.jump(prepared, &[b.param(3)]);
    b.block = prepared;
    let next = b.body.add_block();
    let url = b.body.add_blockparam(next, I32);
    let method = b.body.add_blockparam(next, I32);
    let next_body = b.body.add_blockparam(next, I32);
    let body_kind = b.body.add_blockparam(next, I32);
    let followed = b.body.add_blockparam(next, I32);
    b.jump(next, &[b.param(0), b.param(1), body, b.param(4), zero]);
    b.block = next;
    for (at, value) in [(12, url), (16, method), (24, next_body)] {
        b.store(frame, at, value, I32);
    }
    let result = b.call(
        send,
        &[url, method, headers, next_body, body_kind, three],
        &[I32, F64],
    );
    propagate(&mut b, t, frame, &result);
    let response = b.op(O::I32TruncF64U, &[result[1]], I32);
    b.store(frame, 32, response, I32);
    let status = b.load(response, 0, I32);
    let mut redirect = zero;
    for code in [301, 302, 303, 307, 308] {
        let code = b.integer(code);
        let matches = b.op(O::I32Eq, &[status, code], I32);
        redirect = b.op(O::I32Or, &[redirect, matches], I32);
    }
    let manual = b.op(O::I32Eq, &[mode, one], I32);
    let automatic = b.op(O::I32Eqz, &[manual], I32);
    redirect = b.op(O::I32And, &[redirect, automatic], I32);
    let inspect = b.body.add_block();
    let finished = b.body.add_block();
    b.branch(redirect, inspect, finished);
    b.block = finished;
    let redirected = b.op(O::I32Ne, &[followed, zero], I32);
    b.store(response, 64, redirected, I32);
    b.call(t.allocator.frame_drop, &[frame], &[]);
    b.ret(&result);
    b.block = inspect;
    let fail = b.body.add_block();
    let location = b.body.add_block();
    let error_mode = b.op(O::I32Eq, &[mode, two], I32);
    b.branch(error_mode, fail, location);
    b.block = fail;
    b.call(discard, &[response], &[I32, F64]);
    b.call(t.allocator.frame_drop, &[frame], &[]);
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = location;
    let name = b.integer(r.pool.get("location").unwrap());
    let result = b.call(t.headers.lookup, &[response, name, zero], &[I32, F64]);
    // The static name is valid; failures indicate a corrupt internal field list.
    let valid = b.op(O::I32Eqz, &[result[0]], I32);
    b.require(valid);
    let value = b.op(O::I32TruncF64U, &[result[1]], I32);
    b.store(frame, 36, value, I32);
    let tag = b.load(value, 0, I32);
    let missing = b.op(O::I32Eq, &[tag, one], I32);
    let resolve = b.body.add_block();
    b.branch(missing, finished, resolve);
    b.block = resolve;
    let limit = b.integer(20);
    let exhausted = b.op(O::I32GeU, &[followed, limit], I32);
    let within_limit = b.body.add_block();
    b.branch(exhausted, fail, within_limit);
    b.block = within_limit;
    let payload = b.load(value, 8, F64);
    let location = b.op(O::I32TruncF64U, &[payload], I32);
    let location_data = b.load(location, 0, I32);
    let location_length = b.load(location, 4, I32);
    let base = b.load(response, 16, I32);
    let base_data = b.load(base, 0, I32);
    let base_length = b.load(base, 4, I32);
    let maximum = b.integer((u32::MAX - 128) / 10);
    for length in [base_length, location_length] {
        let fits = b.op(O::I32LeU, &[length, maximum], I32);
        b.require(fits);
    }
    let combined = b.op(O::I32Add, &[base_length, location_length], I32);
    let five = b.integer(5);
    let capacity = b.op(O::I32Mul, &[combined, five], I32);
    let capacity = offset(&mut b, capacity, 128);
    let output = b.call(t.allocator.realloc, &[zero, zero, one, capacity], &[I32])[0];
    b.store(frame, 40, output, I32);
    let invalid = b.call(
        t.native["fetch_redirect"],
        &[
            base_data,
            base_length,
            location_data,
            location_length,
            output,
            capacity,
        ],
        &[I32],
    )[0];
    let resolved = b.body.add_block();
    b.branch(invalid, fail, resolved);
    b.block = resolved;
    let data = offset(&mut b, output, 32);
    let length = b.load(output, 16, I32);
    let target = b.call(r.strings.lift_canonical, &[data, length], &[I32])[0];
    b.store(frame, 44, target, I32);
    let closed = b.call(discard, &[response], &[I32, F64]);
    propagate(&mut b, t, frame, &closed);
    let explicit = b.body.add_block();
    let default = b.body.add_block();
    let classified = b.body.add_block();
    let method_tag = b.body.add_blockparam(classified, I32);
    b.branch(method, explicit, default);
    b.block = default;
    b.jump(classified, &[zero]);
    b.block = explicit;
    let data = b.load(method, 0, I32);
    let length = b.load(method, 4, I32);
    let tag = b.call(t.native["fetch_method"], &[data, length], &[I32])[0];
    b.jump(classified, &[tag]);
    b.block = classified;
    let post = b.op(O::I32Eq, &[method_tag, two], I32);
    let s302 = b.integer(302);
    let s303 = b.integer(303);
    let old_post = b.op(O::I32LeU, &[status, s302], I32);
    let rewrite_post = b.op(O::I32And, &[post, old_post], I32);
    let see_other = b.op(O::I32Eq, &[status, s303], I32);
    let not_get_head = b.op(O::I32GtU, &[method_tag, one], I32);
    let rewrite_other = b.op(O::I32And, &[see_other, not_get_head], I32);
    let rewrite = b.op(O::I32Or, &[rewrite_post, rewrite_other], I32);
    let remove_body = b.body.add_block();
    let origin = b.body.add_block();
    b.branch(rewrite, remove_body, origin);
    b.block = remove_body;
    for key in [
        "content-encoding",
        "content-language",
        "content-location",
        "content-type",
    ] {
        let key = b.integer(r.pool.get(key).unwrap());
        let result = b.call(t.headers.edit, &[headers, key, zero, two], &[I32, F64]);
        propagate(&mut b, t, frame, &result);
    }
    b.jump(origin, &[]);
    b.block = origin;
    let same_origin = b.load(output, 20, I32);
    let foreign = b.body.add_block();
    let repeat = b.body.add_block();
    b.branch(same_origin, repeat, foreign);
    b.block = foreign;
    for key in ["authorization", "proxy-authorization", "cookie", "host"] {
        let key = b.integer(r.pool.get(key).unwrap());
        let result = b.call(t.headers.edit, &[headers, key, zero, two], &[I32, F64]);
        propagate(&mut b, t, frame, &result);
    }
    b.jump(repeat, &[]);
    b.block = repeat;
    let method = b.op(O::Select, &[zero, method, rewrite], I32);
    let body = b.op(O::Select, &[zero, next_body, rewrite], I32);
    let kind = b.op(O::Select, &[zero, body_kind, rewrite], I32);
    let count = b.op(O::I32Add, &[followed, one], I32);
    b.store(frame, 32, zero, I32);
    b.store(frame, 36, zero, I32);
    b.jump(next, &[target, method, body, kind, count]);
    b.finish(module, function)
}
