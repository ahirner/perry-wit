//! Shared single-consumption byte, UTF-8, and JSON body methods.
use crate::waffle_backend::{
    allocation::AllocationFuncs,
    json::JsonHelpers,
    runtime::builder::{self, Builder},
    strings::StringHelperFuncs,
};
use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BodyMethod {
    Bytes,
    Text,
    Json,
    ArrayBuffer,
}
impl BodyMethod {
    pub(crate) const ALL: [Self; 4] = [Self::Bytes, Self::Text, Self::Json, Self::ArrayBuffer];
    pub(crate) fn named(name: &str) -> Option<Self> {
        match name {
            "bytes" => Some(Self::Bytes),
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "arrayBuffer" => Some(Self::ArrayBuffer),
            _ => None,
        }
    }
    pub(crate) fn result(self) -> perry_hir::types::Type {
        match self {
            Self::Text => perry_hir::types::Type::String,
            Self::Json => crate::waffle_backend::values::value_type(),
            Self::ArrayBuffer => perry_hir::types::Type::Named("ArrayBuffer".into()),
            Self::Bytes => perry_hir::types::Type::Named("Uint8Array".into()),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Helpers {
    bytes: Func,
    text: Func,
    json: Option<Func>,
}
impl Helpers {
    pub(crate) fn method(self, method: BodyMethod) -> Func {
        match method {
            BodyMethod::Bytes | BodyMethod::ArrayBuffer => self.bytes,
            BodyMethod::Text => self.text,
            BodyMethod::Json => self.json.unwrap(),
        }
    }
}
pub(crate) struct Runtime {
    pub(crate) allocator: AllocationFuncs,
    pub(crate) strings: StringHelperFuncs,
    pub(crate) json: Option<JsonHelpers>,
    pub(crate) decode: Func,
    pub(crate) bytes: crate::waffle_backend::bytes::ByteHelpers,
    pub(crate) streams: crate::waffle_backend::streams::web::Helpers,
}
pub(crate) fn emit(module: &mut Module<'static>, memory: Memory, r: &Runtime) -> Result<Helpers> {
    let allocator = r.allocator;
    let strings = r.strings;
    let body = builder::declare(module, "body.bytes", &[I32; 2], &[I32, F64]);
    let text = builder::declare(module, "body.text", &[I32; 2], &[I32, F64]);
    emit_bytes(module, memory, body, r)?;
    let mut b = Builder::new(module, text, memory);
    let result = b.call(body, &[b.param(0), b.param(1)], &[I32, F64]);
    let failed = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(result[0], failed, ready);
    b.block = failed;
    b.ret(&result);
    b.block = ready;
    let view = b.op(O::I32TruncF64U, &[result[1]], I32);
    let count = b.integer(2);
    let frame = b.call(allocator.frame_new, &[count], &[I32])[0];
    b.store(frame, 12, view, I32);
    let data = b.load(view, 0, I32);
    let length = b.load(view, 4, I32);
    let max = b.integer(u32::MAX / 3);
    let fits = b.op(O::I32LeU, &[length, max], I32);
    b.require(fits);
    let three = b.integer(3);
    let capacity = b.op(O::I32Mul, &[length, three], I32);
    let zero = b.integer(0);
    let one = b.integer(1);
    let output = b.call(allocator.realloc, &[zero, zero, one, capacity], &[I32])[0];
    b.store(frame, 16, output, I32);
    let length = b.call(r.decode, &[data, length, output, capacity], &[I32])[0];
    let invalid = b.integer(u32::MAX);
    let valid = b.op(O::I32Ne, &[length, invalid], I32);
    b.require(valid);
    let descriptor = b.call(strings.lift_canonical, &[output, length], &[I32])[0];
    b.call(allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[descriptor], F64);
    b.ret(&[zero, payload]);
    b.finish(module, text)?;
    let json = r
        .json
        .map(|json| {
            let function = builder::declare(module, "body.json", &[I32; 2], &[I32, F64]);
            let mut b = Builder::new(module, function, memory);
            let text_result = b.call(text, &[b.param(0), b.param(1)], &[I32, F64]);
            let failed = b.body.add_block();
            let parse = b.body.add_block();
            b.branch(text_result[0], failed, parse);
            b.block = failed;
            b.ret(&text_result);
            b.block = parse;
            let text = b.op(O::I32TruncF64U, &[text_result[1]], I32);
            let one = b.integer(1);
            let frame = b.call(allocator.frame_new, &[one], &[I32])[0];
            b.store(frame, 12, text, I32);
            let result = b.call(json.parse, &[text], &[I32, F64]);
            b.call(allocator.frame_drop, &[frame], &[]);
            b.ret(&result);
            b.finish(module, function)?;
            Ok::<_, anyhow::Error>(function)
        })
        .transpose()?;

    Ok(Helpers {
        bytes: body,
        text,
        json,
    })
}

/// Materialize through byte-stream demand; never reserve an advertised HTTP length.
fn emit_bytes(module: &mut Module<'static>, memory: Memory, f: Func, r: &Runtime) -> Result<()> {
    use crate::waffle_backend::streams::web;
    let mut b = Builder::new(module, f, memory);
    let receiver = b.param(0);
    let kind = b.param(1);
    let zero = b.integer(0);
    let one = b.integer(1);
    let stream = b.call(r.streams.body, &[receiver, kind], &[I32])[0];
    let consume = b.body.add_block();
    let empty = b.body.add_block();
    b.branch(stream, consume, empty);
    b.block = empty;
    let length = b.number(0.0);
    let result = b.call(r.bytes.new, &[length], &[I32, F64]);
    b.ret(&result);
    b.block = consume;
    let locked = b.load(stream, web::LOCK, I32);
    let request_used = b.integer(super::request::BODY_USED);
    let response_used = b.integer(20);
    let offset = b.op(O::Select, &[response_used, request_used, kind], I32);
    let used_address = b.op(O::I32Add, &[receiver, offset], I32);
    let used = b.load(used_address, 0, I32);
    let unusable = b.op(O::I32Or, &[locked, used], I32);
    let reject = b.body.add_block();
    let start = b.body.add_block();
    b.branch(unusable, reject, start);
    b.block = reject;
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = start;
    b.store(stream, web::LOCK, one, I32);
    let count = b.integer(3);
    let frame = b.call(r.allocator.frame_new, &[count], &[I32])[0];
    b.store(frame, 12, stream, I32);
    let next = b.body.add_block();
    let data = b.body.add_blockparam(next, I32);
    let capacity = b.body.add_blockparam(next, I32);
    let length = b.body.add_blockparam(next, I32);
    b.jump(next, &[zero, zero, zero]);
    b.block = next;
    b.store(frame, 16, data, I32);
    b.store(frame, 20, zero, I32);
    let read = b.call(r.streams.pull, &[stream], &[I32; 3]);
    let failed = b.body.add_block();
    let received = b.body.add_block();
    b.branch(read[0], failed, received);
    b.block = failed;
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let reason = b.op(O::F64ConvertI32U, &[read[0]], F64);
    b.ret(&[one, reason]);
    b.block = received;
    let done = b.body.add_block();
    let chunk = b.body.add_block();
    b.branch(read[2], done, chunk);
    b.block = done;
    let view = b.call(r.bytes.lift_canonical, &[data, length], &[I32])[0];
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[view], F64);
    b.ret(&[zero, payload]);
    b.block = chunk;
    b.store(frame, 20, read[1], I32);
    let source = b.load(read[1], 0, I32);
    let count = b.load(read[1], 4, I32);
    let first = b.body.add_block();
    let append = b.body.add_block();
    b.branch(length, append, first);
    b.block = first;
    b.jump(next, &[source, count, count]);
    b.block = append;
    let needed = b.op(O::I32Add, &[length, count], I32);
    let fits = b.op(O::I32GeU, &[needed, length], I32);
    b.require(fits);
    let grow = b.op(O::I32GtU, &[needed, capacity], I32);
    let allocate = b.body.add_block();
    let keep = b.body.add_block();
    let copy = b.body.add_block();
    let target = b.body.add_blockparam(copy, I32);
    let target_capacity = b.body.add_blockparam(copy, I32);
    b.branch(grow, allocate, keep);
    b.block = keep;
    b.jump(copy, &[data, capacity]);
    b.block = allocate;
    let double = b.op(O::I32Add, &[capacity, capacity], I32);
    let within = b.op(O::I32GeU, &[double, capacity], I32);
    let enough = b.op(O::I32GeU, &[double, needed], I32);
    let usable = b.op(O::I32And, &[within, enough], I32);
    let size = b.op(O::Select, &[double, needed, usable], I32);
    let larger = b.call(r.allocator.realloc, &[data, capacity, one, size], &[I32])[0];
    b.jump(copy, &[larger, size]);
    b.block = copy;
    let end = b.op(O::I32Add, &[target, length], I32);
    b.effect(
        O::MemoryCopy {
            src_mem: memory,
            dst_mem: memory,
        },
        &[end, source, count],
    );
    b.jump(next, &[target, target_capacity, needed]);
    b.finish(module, f)
}
