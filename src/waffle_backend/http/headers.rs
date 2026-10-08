//! Owned standard header results from the allocation-free canonical field codec.

use crate::waffle_backend::runtime::builder::{self, Builder};
use crate::waffle_backend::{
    allocation::AllocationFuncs, runtime::imports, strings::StringHelperFuncs, values::ValueHelpers,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
};

pub(crate) const HEADERS_TYPE: &str = "__perry_fetch_headers";
pub(crate) fn is_headers(ty: &perry_hir::types::Type) -> bool {
    matches!(ty, perry_hir::types::Type::Named(name) if name == HEADERS_TYPE)
}

fn emit_lookup(
    module: &mut Module<'static>,
    memory: Memory,
    runtime: &Runtime<'_>,
) -> Result<Func> {
    let function = builder::declare(module, "headers.lookup", &[I32; 3], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let response = b.param(0);
    let name = b.param(1);
    let has = b.param(2);
    let fields = b.load(response, 4, I32);
    let count = b.load(response, 8, I32);
    let data = b.load(name, 0, I32);
    let length = b.load(name, 4, I32);
    let size = b.call(
        runtime.imports["fetch_header_size"],
        &[fields, count, data, length],
        &[I32],
    )[0];
    let zero = b.integer(0);
    let one = b.integer(1);
    let invalid = b.integer(u32::MAX - 1);
    let invalid = b.op(O::I32Eq, &[size, invalid], I32);
    let bad = b.body.add_block();
    let accepted = b.body.add_block();
    b.branch(invalid, bad, accepted);
    b.block = bad;
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = accepted;
    let missing = b.integer(u32::MAX);
    let present = b.op(O::I32Ne, &[size, missing], I32);
    let presence = b.body.add_block();
    let get = b.body.add_block();
    b.branch(has, presence, get);
    b.block = presence;
    let payload = b.op(O::F64ConvertI32U, &[present], F64);
    b.ret(&[zero, payload]);
    b.block = get;
    let four = b.integer(4);
    let frame = b.call(runtime.allocator.frame_new, &[four], &[I32])[0];
    b.store(frame, 12, response, I32);
    b.store(frame, 16, name, I32);
    let null = b.body.add_block();
    let copy = b.body.add_block();
    b.branch(present, copy, null);
    b.block = null;
    let payload = b.number(0.0);
    let boxed = b.call(runtime.values.new, &[one, payload], &[I32])[0];
    let payload = b.op(O::F64ConvertI32U, &[boxed], F64);
    b.call(runtime.allocator.frame_drop, &[frame], &[]);
    b.ret(&[zero, payload]);
    b.block = copy;
    let allocation = b.op(O::Select, &[size, one, size], I32);
    let output = b.call(
        runtime.allocator.realloc,
        &[zero, zero, one, allocation],
        &[I32],
    )[0];
    b.store(frame, 20, output, I32);
    let written = b.call(
        runtime.imports["fetch_header_get"],
        &[fields, count, data, length, output, size],
        &[I32],
    )[0];
    let valid = b.op(O::I32Eq, &[written, size], I32);
    b.require(valid);
    let text = b.call(runtime.strings.lift_canonical, &[output, size], &[I32])[0];
    b.store(frame, 24, text, I32);
    let payload = b.op(O::F64ConvertI32U, &[text], F64);
    let boxed = b.call(runtime.values.new, &[four, payload], &[I32])[0];
    let payload = b.op(O::F64ConvertI32U, &[boxed], F64);
    b.call(runtime.allocator.frame_drop, &[frame], &[]);
    b.ret(&[zero, payload]);
    b.finish(module, function)?;
    Ok(function)
}

pub(crate) struct Runtime<'a> {
    pub(crate) allocator: AllocationFuncs,
    pub(crate) strings: StringHelperFuncs,
    pub(crate) values: ValueHelpers,
    pub(crate) imports: &'a BTreeMap<String, Func>,
}
#[derive(Clone, Copy)]
pub(crate) struct Helpers {
    pub(crate) new: Func,
    pub(crate) wrap: Func,
    pub(crate) lookup: Func,
    pub(crate) edit: Func,
    pub(crate) default_type: Func,
}

pub(crate) fn declare_helpers(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    let functions = [
        ("fetch_header_size", 4),
        ("fetch_header_get", 6),
        ("fetch_header_name", 4),
        ("fetch_header_value", 4),
        ("fetch_header_edit", 9),
    ]
    .map(|(name, count)| imports::Function {
        name: name.into(),
        params: vec!["i32"; count],
        results: vec!["i32"],
    });
    imports::declare_imports(
        module,
        crate::waffle_backend::link::HELPER_MODULE,
        &functions,
    )
}

fn reject(b: &mut Builder, r: &Runtime<'_>, frame: waffle::Value, condition: waffle::Value) {
    let bad = b.body.add_block();
    let next = b.body.add_block();
    b.branch(condition, bad, next);
    b.block = bad;
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let one = b.integer(1);
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = next;
}

fn emit_edit(module: &mut Module<'static>, memory: Memory, r: &Runtime<'_>) -> Result<Func> {
    let function = builder::declare(module, "headers.edit", &[I32; 4], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let headers = b.param(0);
    let name = b.param(1);
    let value = b.param(2);
    let action = b.param(3);
    let six = b.integer(6);
    let frame = b.call(r.allocator.frame_new, &[six], &[I32])[0];
    for (at, root) in [(12, headers), (16, name), (20, value)] {
        b.store(frame, at, root, I32);
    }
    let guard = b.load(headers, 0, I32);
    reject(&mut b, r, frame, guard);
    let zero = b.integer(0);
    let one = b.integer(1);
    let invalid = b.integer(u32::MAX);
    let name_data = b.load(name, 0, I32);
    let name_length = b.load(name, 4, I32);
    let capacity = b.op(O::Select, &[name_length, one, name_length], I32);
    let normalized_name = b.call(r.allocator.realloc, &[zero, zero, one, capacity], &[I32])[0];
    b.store(frame, 24, normalized_name, I32);
    let normalized_length = b.call(
        r.imports["fetch_header_name"],
        &[name_data, name_length, normalized_name, capacity],
        &[I32],
    )[0];
    let bad = b.op(O::I32Eq, &[normalized_length, invalid], I32);
    reject(&mut b, r, frame, bad);
    let supplied = b.body.add_block();
    let omitted = b.body.add_block();
    let normalized = b.body.add_block();
    let value_data = b.body.add_blockparam(normalized, I32);
    let value_length = b.body.add_blockparam(normalized, I32);
    b.branch(value, supplied, omitted);
    b.block = omitted;
    b.jump(normalized, &[zero, zero]);
    b.block = supplied;
    let data = b.load(value, 0, I32);
    let length = b.load(value, 4, I32);
    let capacity = b.op(O::Select, &[length, one, length], I32);
    let output = b.call(r.allocator.realloc, &[zero, zero, one, capacity], &[I32])[0];
    b.store(frame, 28, output, I32);
    let length = b.call(
        r.imports["fetch_header_value"],
        &[data, length, output, capacity],
        &[I32],
    )[0];
    let bad = b.op(O::I32Eq, &[length, invalid], I32);
    reject(&mut b, r, frame, bad);
    b.jump(normalized, &[output, length]);
    b.block = normalized;
    let fields = b.load(headers, 4, I32);
    let count = b.load(headers, 8, I32);
    let max = b.integer(u32::MAX / 16);
    let fits = b.op(O::I32LtU, &[count, max], I32);
    b.require(fits);
    let next = b.op(O::I32Add, &[count, one], I32);
    let stride = b.integer(16);
    let capacity = b.op(O::I32Mul, &[next, stride], I32);
    let four = b.integer(4);
    let output = b.call(r.allocator.realloc, &[zero, zero, four, capacity], &[I32])[0];
    b.store(frame, 32, output, I32);
    let count = b.call(
        r.imports["fetch_header_edit"],
        &[
            fields,
            count,
            normalized_name,
            normalized_length,
            value_data,
            value_length,
            action,
            output,
            capacity,
        ],
        &[I32],
    )[0];
    let bad = b.op(O::I32Eq, &[count, invalid], I32);
    reject(&mut b, r, frame, bad);
    b.store(headers, 4, output, I32);
    b.store(headers, 8, count, I32);
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let result = b.number(0.0);
    b.ret(&[zero, result]);
    b.finish(module, function)?;
    Ok(function)
}

pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
) -> Result<Helpers> {
    let edit = emit_edit(module, memory, r)?;
    let lookup = emit_lookup(module, memory, r)?;
    let wrap = builder::declare(module, "headers.wrap", &[I32; 3], &[I32]);
    let mut b = Builder::new(module, wrap, memory);
    let headers = b.allocate(r.allocator.realloc, 16, 4);
    let zero = b.integer(0);
    for (offset, value) in [
        (0, b.param(2)),
        (4, b.param(0)),
        (8, b.param(1)),
        (12, zero),
    ] {
        b.store(headers, offset, value, I32);
    }
    let four = b.integer(4);
    let backlink = b.op(O::I32Sub, &[headers, four], I32);
    let allocation = b.load(backlink, 0, I32);
    let kind = b.integer(13);
    b.store(allocation, 16, kind, I32);
    b.ret(&[headers]);
    b.finish(module, wrap)?;
    let function = builder::declare(module, "headers.new", &[I32; 2], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let mode = b.param(0);
    let input = b.param(1);
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let three = b.integer(3);
    let frame = b.call(r.allocator.frame_new, &[two], &[I32])[0];
    b.store(frame, 12, input, I32);
    let headers = b.call(wrap, &[zero, zero, zero], &[I32])[0];
    b.store(frame, 16, headers, I32);
    let four = b.integer(4);
    let finish = b.body.add_block();
    let nonempty = b.body.add_block();
    b.branch(mode, nonempty, finish);
    b.block = nonempty;
    let copy = b.body.add_block();
    let entries = b.body.add_block();
    let is_copy = b.op(O::I32Eq, &[mode, three], I32);
    b.branch(is_copy, copy, entries);
    b.block = copy;
    for at in [4, 8] {
        let value = b.load(input, at, I32);
        b.store(headers, at, value, I32);
    }
    b.jump(finish, &[]);
    b.block = entries;
    let record = b.body.add_block();
    let pairs = b.body.add_block();
    let is_record = b.op(O::I32Eq, &[mode, one], I32);
    b.branch(is_record, record, pairs);
    b.block = record;
    let head = b.load(input, 0, I32);
    let records = b.body.add_block();
    let entry = b.body.add_blockparam(records, I32);
    b.jump(records, &[head]);
    b.block = records;
    let append = b.body.add_block();
    b.branch(entry, append, finish);
    b.block = append;
    let name = b.load(entry, 4, I32);
    let value = b.load(entry, 16, F64);
    let value = b.op(O::I32TruncF64U, &[value], I32);
    let result = b.call(edit, &[headers, name, value, zero], &[I32, F64]);
    reject(&mut b, r, frame, result[0]);
    let next = b.load(entry, 0, I32);
    b.jump(records, &[next]);
    b.block = pairs;
    let count = b.load(input, 4, I32);
    let data = b.load(input, 0, I32);
    let loop_block = b.body.add_block();
    let index = b.body.add_blockparam(loop_block, I32);
    b.jump(loop_block, &[zero]);
    b.block = loop_block;
    let more = b.op(O::I32LtU, &[index, count], I32);
    let pair = b.body.add_block();
    b.branch(more, pair, finish);
    b.block = pair;
    let offset = b.op(O::I32Mul, &[index, four], I32);
    let slot = b.op(O::I32Add, &[data, offset], I32);
    let boxed = b.load(slot, 0, I32);
    let payload = b.load(boxed, 8, F64);
    let pair = b.op(O::I32TruncF64U, &[payload], I32);
    let elements = b.load(pair, 4, I32);
    let bad = b.op(O::I32Ne, &[elements, two], I32);
    reject(&mut b, r, frame, bad);
    let slots = b.load(pair, 0, I32);
    let mut values = Vec::new();
    for at in [0, 4] {
        let boxed = b.load(slots, at, I32);
        let value = b.load(boxed, 8, F64);
        values.push(b.op(O::I32TruncF64U, &[value], I32));
    }
    let result = b.call(edit, &[headers, values[0], values[1], zero], &[I32, F64]);
    reject(&mut b, r, frame, result[0]);
    let next = b.op(O::I32Add, &[index, one], I32);
    b.jump(loop_block, &[next]);
    b.block = finish;
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[headers], F64);
    b.ret(&[zero, payload]);
    b.finish(module, function)?;
    let default_type = emit_default_type(module, memory, lookup, edit)?;
    Ok(Helpers {
        wrap,
        default_type,
        new: function,
        lookup,
        edit,
    })
}

fn emit_default_type(
    module: &mut Module<'static>,
    memory: Memory,
    lookup: Func,
    edit: Func,
) -> Result<Func> {
    let function = builder::declare(module, "headers.default-type", &[I32; 4], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let one = b.integer(1);
    let text = b.op(O::I32Eq, &[b.param(1), one], I32);
    let inspect = b.body.add_block();
    let done = b.body.add_block();
    b.branch(text, inspect, done);
    b.block = inspect;
    let present = b.call(lookup, &[b.param(0), b.param(2), one], &[I32, F64]);
    let failed = b.body.add_block();
    let valid = b.body.add_block();
    b.branch(present[0], failed, valid);
    b.block = failed;
    b.ret(&present);
    b.block = valid;
    let present = b.op(O::I32TruncF64U, &[present[1]], I32);
    let insert = b.body.add_block();
    b.branch(present, done, insert);
    b.block = insert;
    let result = b.call(
        edit,
        &[b.param(0), b.param(2), b.param(3), one],
        &[I32, F64],
    );
    b.ret(&result);
    b.block = done;
    let zero = b.integer(0);
    let payload = b.number(0.0);
    b.ret(&[zero, payload]);
    b.finish(module, function)?;
    Ok(function)
}
