//! Each operand installs one observer; the last observer releases shared roots.

use super::*;
use crate::waffle_backend::strings::StringPool;
use waffle::Value;

fn root_frame(b: &mut Builder, registry: &ModuleRegistry, values: &[Value]) -> Value {
    let count = b.integer(values.len() as u32);
    let frame = b.call(
        registry.allocator.unwrap().frame_new,
        &[count],
        &[Type::I32],
    )[0];
    for (index, value) in values.iter().enumerate() {
        b.store(frame, 12 + 4 * index as u32, *value, Type::I32);
    }
    frame
}
fn at(b: &mut Builder, base: Value, index: Value, width: u32) -> Value {
    let width = b.integer(width);
    let offset = b.op(Operator::I32Mul, &[index, width], Type::I32);
    b.op(Operator::I32Add, &[base, offset], Type::I32)
}
fn eq(b: &mut Builder, value: Value, constant: u32) -> Value {
    let constant = b.integer(constant);
    b.op(Operator::I32Eq, &[value, constant], Type::I32)
}
fn box_value(b: &mut Builder, registry: &ModuleRegistry, tag: Value, payload: Value) -> Value {
    b.call(
        registry.value_helpers.unwrap().new,
        &[tag, payload],
        &[Type::I32],
    )[0]
}

fn race_direct_value(b: &mut Builder, representation: Value, tag: Value, payload: Value) -> Value {
    use Type::{F64, I32};
    let text_or_bytes = eq(b, representation, 3);
    let binary = eq(b, tag, 5);
    let pointer = b.op(Operator::I32TruncSatF64U, &[payload], I32);
    let pointer = b.op(Operator::I32Or, &[pointer, binary], I32);
    let pointer = b.op(Operator::F64ConvertI32U, &[pointer], F64);
    let value = b.op(Operator::Select, &[pointer, payload, text_or_bytes], F64);
    let optional_number = eq(b, representation, 4);
    let undefined = eq(b, tag, 0);
    let missing_number = b.op(Operator::I32And, &[optional_number, undefined], I32);
    let nan = b.op(
        Operator::F64Const {
            value: f64::NAN.to_bits(),
        },
        &[],
        F64,
    );
    let value = b.op(Operator::Select, &[nan, value, missing_number], F64);
    let optional_string = eq(b, representation, 5);
    let missing_string = b.op(Operator::I32And, &[optional_string, undefined], I32);
    let zero = b.op(Operator::F64Const { value: 0 }, &[], F64);
    b.op(Operator::Select, &[zero, value, missing_string], F64)
}

pub(super) fn emit(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    worker_index: u32,
    strings: &StringPool,
) -> Result<Func> {
    use Type::{F64, I32};
    let runtime = registry.promises.as_ref().unwrap();
    let native = runtime.native.as_ref().unwrap();
    let combine = native.combine.unwrap();
    let worker = builder::declare(module, "tasks.observe", &[I32], &[]);
    let mut b = Builder::new(module, combine, registry.memory);
    let mode = b.param(0);
    let input = b.param(1);
    let tags = b.param(2);
    let kind = b.param(3);
    let strings_output = b.param(4);
    let strings_input = b.param(5);
    let aggregate = b.call(runtime.new, &[kind], &[I32])[0];
    let count = b.load(input, 4, I32);
    let results = b.call(registry.value_access.unwrap().array_new, &[count], &[I32])[0];
    let meta = allocate(&mut b, registry, 24);
    let roots = root_frame(&mut b, registry, &[aggregate, results, meta]);
    for (offset, value) in [
        (0, aggregate),
        (4, results),
        (8, count),
        (12, mode),
        (16, roots),
        (20, strings_output),
    ] {
        b.store(meta, offset, value, I32);
    }
    let zero = b.integer(0);
    let next = b.body.add_block();
    let index = b.body.add_blockparam(next, I32);
    let register = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[zero]);
    b.block = next;
    let more = b.op(Operator::I32LtU, &[index, count], I32);
    b.branch(more, register, done);
    b.block = register;
    let data = b.load(input, 0, I32);
    let string = b.body.add_block();
    let boxed = b.body.add_block();
    let value_ready = b.body.add_block();
    let input_value = b.body.add_blockparam(value_ready, I32);
    b.branch(strings_input, string, boxed);
    b.block = string;
    let descriptor = at(&mut b, data, index, 12);
    let payload = b.op(Operator::F64ConvertI32U, &[descriptor], F64);
    let string_tag = b.integer(4);
    let value = box_value(&mut b, registry, string_tag, payload);
    b.jump(value_ready, &[value]);
    b.block = boxed;
    let slot = at(&mut b, data, index, 4);
    let value = b.load(slot, 0, I32);
    b.jump(value_ready, &[value]);
    b.block = value_ready;
    let scalar_tags = b.integer(256);
    let is_scalar = b.op(Operator::I32LtU, &[tags, scalar_tags], I32);
    let scalar = b.body.add_block();
    let tuple = b.body.add_block();
    let typed = b.body.add_block();
    let outcome_tag = b.body.add_blockparam(typed, I32);
    b.branch(is_scalar, scalar, tuple);
    b.block = scalar;
    b.jump(typed, &[tags]);
    b.block = tuple;
    let tag_data = b.load(tags, 0, I32);
    let tag_slot = at(&mut b, tag_data, index, 4);
    let tag_box = b.load(tag_slot, 0, I32);
    let payload = b.load(tag_box, 8, F64);
    let tag = b.op(Operator::I32TruncF64U, &[payload], I32);
    b.jump(typed, &[tag]);
    b.block = typed;
    let context = allocate(&mut b, registry, 24);
    let frame = root_frame(&mut b, registry, &[context, input_value]);
    for (offset, value) in [
        (0, meta),
        (4, input_value),
        (8, outcome_tag),
        (12, index),
        (16, frame),
    ] {
        b.store(context, offset, value, I32);
    }
    let worker_index = b.integer(worker_index);
    let thread = b.call(native.new_thread, &[worker_index, context], &[I32])[0];
    let input_tag = b.load(input_value, 0, I32);
    let promise = eq(&mut b, input_tag, 10);
    let observe = b.body.add_block();
    let enqueue = b.body.add_block();
    let registered = b.body.add_block();
    b.branch(promise, observe, enqueue);
    b.block = observe;
    let payload = b.load(input_value, 8, F64);
    let record = b.op(Operator::I32TruncF64U, &[payload], I32);
    b.call(native.observe, &[record, thread], &[]);
    b.jump(registered, &[]);
    b.block = enqueue;
    b.call(native.enqueue, &[thread], &[]);
    b.jump(registered, &[]);
    b.block = registered;
    let one = b.integer(1);
    let following = b.op(Operator::I32Add, &[index, one], I32);
    b.jump(next, &[following]);
    b.block = done;
    let empty = b.op(Operator::I32Eqz, &[count], I32);
    let no_operands = b.body.add_block();
    let returned = b.body.add_block();
    b.branch(empty, no_operands, returned);
    b.block = no_operands;
    let race = eq(&mut b, mode, 2);
    let release = b.body.add_block();
    let settle = b.body.add_block();
    b.branch(race, release, settle);
    b.block = settle;
    let payload = b.op(Operator::F64ConvertI32U, &[results], F64);
    b.call(native.settle, &[aggregate, zero, payload], &[]);
    b.jump(release, &[]);
    b.block = release;
    b.call(registry.allocator.unwrap().frame_drop, &[roots], &[]);
    b.jump(returned, &[]);
    b.block = returned;
    b.ret(&[aggregate]);
    b.finish(module, combine)?;
    emit_observer(module, registry, worker, strings)?;
    Ok(worker)
}

fn emit_observer(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    worker: Func,
    strings: &StringPool,
) -> Result<()> {
    use Type::{F64, I32};
    let runtime = registry.promises.as_ref().unwrap();
    let native = runtime.native.as_ref().unwrap();
    let mut b = Builder::new(module, worker, registry.memory);
    let context = b.param(0);
    let meta = b.load(context, 0, I32);
    let input = b.load(context, 4, I32);
    let outcome_tag = b.load(context, 8, I32);
    let index = b.load(context, 12, I32);
    let frame = b.load(context, 16, I32);
    let input_tag = b.load(input, 0, I32);
    let input_payload = b.load(input, 8, F64);
    let promise = eq(&mut b, input_tag, 10);
    let wait = b.body.add_block();
    let plain = b.body.add_block();
    let observed = b.body.add_block();
    let status = b.body.add_blockparam(observed, I32);
    let payload = b.body.add_blockparam(observed, F64);
    b.branch(promise, wait, plain);
    b.block = wait;
    let record = b.op(Operator::I32TruncF64U, &[input_payload], I32);
    let record_status = b.load(record, 4, I32);
    let two = b.integer(2);
    let settled = b.op(Operator::I32LtU, &[record_status, two], I32);
    b.require(settled);
    let record_payload = b.load(record, 8, F64);
    b.jump(observed, &[record_status, record_payload]);
    b.block = plain;
    let zero = b.integer(0);
    b.jump(observed, &[zero, input_payload]);
    b.block = observed;
    let tagged = eq(&mut b, outcome_tag, 255);
    let success = b.op(Operator::I32Eqz, &[status], I32);
    let tagged = b.op(Operator::I32And, &[tagged, success], I32);
    let extract = b.body.add_block();
    let unchanged = b.body.add_block();
    let typed = b.body.add_block();
    let typed_tag = b.body.add_blockparam(typed, I32);
    let typed_payload = b.body.add_blockparam(typed, F64);
    b.branch(tagged, extract, unchanged);
    b.block = extract;
    let pointer = b.op(Operator::I32TruncF64U, &[payload], I32);
    let tag = b.load(pointer, 0, I32);
    let value = b.load(pointer, 8, F64);
    b.jump(typed, &[tag, value]);
    b.block = unchanged;
    b.jump(typed, &[outcome_tag, payload]);
    b.block = typed;
    let outcome_tag = typed_tag;
    let payload = typed_payload;
    let aggregate = b.load(meta, 0, I32);
    let results = b.load(meta, 4, I32);
    let mode = b.load(meta, 12, I32);
    let state = b.load(aggregate, 4, I32);
    let pending = eq(&mut b, state, 2);
    let apply = b.body.add_block();
    let release = b.body.add_block();
    b.branch(pending, apply, release);
    b.block = apply;
    let race = eq(&mut b, mode, 2);
    let settle_race = b.body.add_block();
    let collect = b.body.add_block();
    b.branch(race, settle_race, collect);
    b.block = settle_race;
    let representation = b.load(meta, 20, I32);
    let tagged = eq(&mut b, representation, 2);
    let success = b.op(Operator::I32Eqz, &[status], I32);
    let tagged = b.op(Operator::I32And, &[tagged, success], I32);
    let box_result = b.body.add_block();
    let direct_result = b.body.add_block();
    let race_ready = b.body.add_block();
    let race_payload = b.body.add_blockparam(race_ready, F64);
    b.branch(tagged, box_result, direct_result);
    b.block = box_result;
    let boxed = box_value(&mut b, registry, outcome_tag, payload);
    let value = b.op(Operator::F64ConvertI32U, &[boxed], F64);
    b.jump(race_ready, &[value]);
    b.block = direct_result;
    let value = race_direct_value(&mut b, representation, outcome_tag, payload);
    let value = b.op(Operator::Select, &[payload, value, status], F64);
    b.jump(race_ready, &[value]);
    b.block = race_ready;
    b.call(native.settle, &[aggregate, status, race_payload], &[]);
    b.jump(release, &[]);
    b.block = collect;
    let all_settled = eq(&mut b, mode, 1);
    let settlement = b.body.add_block();
    let all = b.body.add_block();
    let store_value = b.body.add_block();
    let boxed_value = b.body.add_blockparam(store_value, I32);
    b.branch(all_settled, settlement, all);
    b.block = all;
    let failure = b.body.add_block();
    let success = b.body.add_block();
    b.branch(status, failure, success);
    b.block = failure;
    b.call(native.settle, &[aggregate, status, payload], &[]);
    b.jump(release, &[]);
    b.block = success;
    let boxed = box_value(&mut b, registry, outcome_tag, payload);
    b.jump(store_value, &[boxed]);
    b.block = settlement;
    let object = b.call(registry.object_helpers.unwrap().new, &[], &[I32])[0];
    let failed = b.body.add_block();
    let fulfilled = b.body.add_block();
    let object_ready = b.body.add_block();
    b.branch(status, failed, fulfilled);
    for (block, label, field) in [
        (failed, "rejected", "reason"),
        (fulfilled, "fulfilled", "value"),
    ] {
        b.block = block;
        let key = b.integer(strings.get("status").unwrap());
        let value = b.integer(strings.get(label).unwrap());
        let value = b.op(Operator::F64ConvertI32U, &[value], F64);
        let string_tag = b.integer(4);
        b.call(
            registry.object_helpers.unwrap().set,
            &[object, key, string_tag, value],
            &[I32, F64],
        );
        let key = b.integer(strings.get(field).unwrap());
        let tag = if field == "reason" {
            b.integer(3)
        } else {
            outcome_tag
        };
        b.call(
            registry.object_helpers.unwrap().set,
            &[object, key, tag, payload],
            &[I32, F64],
        );
        b.jump(object_ready, &[]);
    }
    b.block = object_ready;
    let tag = b.integer(6);
    let object = b.op(Operator::F64ConvertI32U, &[object], F64);
    let boxed = box_value(&mut b, registry, tag, object);
    b.jump(store_value, &[boxed]);
    b.block = store_value;
    let data = b.load(results, 0, I32);
    let slot = at(&mut b, data, index, 4);
    b.store(slot, 0, boxed_value, I32);
    b.jump(release, &[]);
    b.block = release;
    let remaining = b.load(meta, 8, I32);
    let one = b.integer(1);
    let remaining = b.op(Operator::I32Sub, &[remaining, one], I32);
    b.store(meta, 8, remaining, I32);
    let last = b.body.add_block();
    let done = b.body.add_block();
    b.branch(remaining, done, last);
    b.block = last;
    let state = b.load(aggregate, 4, I32);
    let pending = eq(&mut b, state, 2);
    let settle_all = b.body.add_block();
    let drop_shared = b.body.add_block();
    b.branch(pending, settle_all, drop_shared);
    b.block = settle_all;
    let strings_output = b.load(meta, 20, I32);
    let convert = b.body.add_block();
    let values = b.body.add_block();
    let result_ready = b.body.add_block();
    let final_result = b.body.add_blockparam(result_ready, I32);
    b.branch(strings_output, convert, values);
    b.block = values;
    b.jump(result_ready, &[results]);
    b.block = convert;
    let result = string_results(&mut b, registry, results);
    b.jump(result_ready, &[result]);
    b.block = result_ready;
    let zero = b.integer(0);
    let payload = b.op(Operator::F64ConvertI32U, &[final_result], F64);
    b.call(native.settle, &[aggregate, zero, payload], &[]);
    b.jump(drop_shared, &[]);
    b.block = drop_shared;
    let shared_frame = b.load(meta, 16, I32);
    b.call(registry.allocator.unwrap().frame_drop, &[shared_frame], &[]);
    b.jump(done, &[]);
    b.block = done;
    b.call(registry.allocator.unwrap().frame_drop, &[frame], &[]);
    b.call(native.complete, &[], &[]);
    b.ret(&[]);
    b.finish(module, worker)
}

fn string_results(b: &mut Builder, registry: &ModuleRegistry, results: Value) -> Value {
    use Type::{F64, I32};
    let count = b.load(results, 4, I32);
    let max = b.integer((u32::MAX - 8) / 12);
    let bounded = b.op(Operator::I32LeU, &[count, max], I32);
    b.require(bounded);
    let eight = b.integer(8);
    let size = at(b, eight, count, 12);
    let zero = b.integer(0);
    let four = b.integer(4);
    let array = b.call(
        registry.allocator.unwrap().realloc,
        &[zero, zero, four, size],
        &[I32],
    )[0];
    let header = b.op(Operator::I32Sub, &[array, four], I32);
    let header = b.load(header, 0, I32);
    let kind = b.integer(2);
    b.store(header, 16, kind, I32);
    let data = b.op(Operator::I32Add, &[array, eight], I32);
    b.store(array, 0, data, I32);
    b.store(array, 4, count, I32);
    let source = b.load(results, 0, I32);
    let next = b.body.add_block();
    let index = b.body.add_blockparam(next, I32);
    let copy = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[zero]);
    b.block = next;
    let more = b.op(Operator::I32LtU, &[index, count], I32);
    b.branch(more, copy, done);
    b.block = copy;
    let slot = at(b, source, index, 4);
    let boxed = b.load(slot, 0, I32);
    let payload = b.load(boxed, 8, F64);
    let descriptor = b.op(Operator::I32TruncF64U, &[payload], I32);
    let target = at(b, data, index, 12);
    let bytes = b.integer(12);
    b.effect(
        Operator::MemoryCopy {
            src_mem: registry.memory,
            dst_mem: registry.memory,
        },
        &[target, descriptor, bytes],
    );
    let one = b.integer(1);
    let following = b.op(Operator::I32Add, &[index, one], I32);
    b.jump(next, &[following]);
    b.block = done;
    array
}
