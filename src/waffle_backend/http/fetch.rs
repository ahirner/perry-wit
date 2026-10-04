//! Headers-first fetch and separately owned body consumption over official P3 HTTP.

use crate::waffle_backend::{
    allocation::AllocationFuncs,
    bytes::ByteHelpers,
    runtime::{
        builder::{self, Builder},
        imports,
    },
    streams,
    strings::StringHelperFuncs,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32, I64},
    Value,
};

mod request;

pub(crate) const RESPONSE_TYPE: &str = "__perry_fetch_response";
pub(crate) const OWNERS: u32 = 104;

pub(crate) fn is_response(ty: &perry_hir::types::Type) -> bool {
    matches!(ty, perry_hir::types::Type::Named(name) if name == RESPONSE_TYPE)
}

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
    pub(crate) fetch: Func,
    pub(crate) upload: Func,
    pub(crate) bytes: Func,
    pub(crate) text: Func,
    json: Option<Func>,
    pub(crate) headers: Func,
    pub(crate) finish: Func,
}
impl Helpers {
    pub(crate) fn body(self, method: BodyMethod) -> Func {
        match method {
            BodyMethod::Bytes | BodyMethod::ArrayBuffer => self.bytes,
            BodyMethod::Text => self.text,
            BodyMethod::Json => self.json.unwrap(),
        }
    }
}

fn offset(b: &mut Builder, pointer: Value, offset: u32) -> Value {
    let offset = b.integer(offset);
    b.op(O::I32Add, &[pointer, offset], I32)
}
fn low(b: &mut Builder, pair: Value) -> Value {
    b.op(O::I32WrapI64, &[pair], I32)
}
fn high(b: &mut Builder, pair: Value) -> Value {
    let shift = b.op(O::I64Const { value: 32 }, &[], I64);
    let high = b.op(O::I64ShrU, &[pair, shift], I64);
    low(b, high)
}
fn byte(b: &mut Builder, address: Value, offset: u32) -> Value {
    b.op(
        O::I32Load8U {
            memory: b.memory(offset),
        },
        &[address],
        I32,
    )
}
fn reject(b: &mut Builder, condition: Value, code: u32) {
    let bad = b.body.add_block();
    let next = b.body.add_block();
    b.branch(condition, bad, next);
    b.block = bad;
    let one = b.integer(1);
    let error = b.number(f64::from(code));
    b.ret(&[one, error]);
    b.block = next;
}
fn error_code(b: &mut Builder, scratch: Value) -> Value {
    let failed = byte(b, scratch, 0);
    let code = byte(b, scratch, 8);
    let base = b.integer(100);
    let code = b.op(O::I32Add, &[code, base], I32);
    let zero = b.integer(0);
    b.op(O::Select, &[code, zero, failed], I32)
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    runtime: &super::SourceRuntime<'_>,
) -> Result<Helpers> {
    let allocator = runtime.allocator;
    let strings = runtime.strings;
    let bytes = runtime.bytes;
    let native = runtime.imports;
    let promises = runtime.promises.unwrap();
    let headers = super::headers::emit(module, memory, runtime)?;
    let fetch = builder::declare(module, "fetch", &[I32; 5], &[I32, F64]);
    let body = builder::declare(module, "fetch.consume", &[I32], &[I32, F64]);
    let text = builder::declare(module, "fetch.text", &[I32], &[I32, F64]);
    let finish = builder::declare(module, "fetch.finish", &[], &[]);
    let transport = Transport {
        native,
        allocator,
        finish_write: super::future::emit_finish_write(module, memory, native)?,
        promises,
        consume: body,
        content_type: [
            runtime.pool.get("content-type").unwrap(),
            runtime.pool.get("text/plain;charset=UTF-8").unwrap(),
        ],
    };
    emit_fetch(
        module,
        memory,
        fetch,
        &transport,
        strings,
        native["fetch_url"],
        bytes,
    )?;
    let upload = request::emit_upload(module, memory, native)?;
    let transfer = streams::emit_read_transfer(module, memory, native["read"])?;
    let buffered = streams::buffered::emit(module, memory, allocator, transfer)?;
    emit_body(module, memory, body, &transport, bytes, buffered)?;
    let mut b = Builder::new(module, text, memory);
    let result = b.call(body, &[b.param(0)], &[I32, F64]);
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
    let length = b.call(
        native["fetch_decode"],
        &[data, length, output, capacity],
        &[I32],
    )[0];
    let invalid = b.integer(u32::MAX);
    let valid = b.op(O::I32Ne, &[length, invalid], I32);
    b.require(valid);
    let descriptor = b.call(strings.lift_canonical, &[output, length], &[I32])[0];
    b.call(allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[descriptor], F64);
    b.ret(&[zero, payload]);
    b.finish(module, text)?;
    let json = runtime
        .json
        .map(|json| {
            let function = builder::declare(module, "fetch.json", &[I32], &[I32, F64]);
            let mut b = Builder::new(module, function, memory);
            let text_result = b.call(text, &[b.param(0)], &[I32, F64]);
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
    let mut b = Builder::new(module, finish, memory);
    let address = b.integer(OWNERS);
    let pending = b.load(address, 0, I32);
    let clear = b.op(O::I32Eqz, &[pending], I32);
    b.require(clear);
    b.ret(&[]);
    b.finish(module, finish)?;
    Ok(Helpers {
        fetch,
        headers,
        upload,
        bytes: body,
        text,
        json,
        finish,
    })
}

struct Transport<'a> {
    native: &'a BTreeMap<String, Func>,
    allocator: AllocationFuncs,
    finish_write: Func,
    consume: Func,
    content_type: [u32; 2],
    promises: &'a crate::waffle_backend::registry::PromiseImports,
}
impl Transport<'_> {
    fn finish_request(&self, b: &mut Builder, response: Value, scratch: Value) -> Value {
        let upload = b.load(response, 60, I32);
        let join = b.body.add_block();
        let complete = b.body.add_block();
        b.branch(upload, join, complete);
        b.block = join;
        b.call(self.promises.await_result, &[upload], &[I32, F64]);
        b.jump(complete, &[]);
        b.block = complete;
        let reader = b.load(response, 36, I32);
        let status = b.call(self.native["read-completion"], &[reader, scratch], &[I32])[0];
        let done = b.op(O::I32Eqz, &[status], I32);
        b.require(done);
        b.call(self.native["drop-completion-reader"], &[reader], &[]);
        let error = error_code(b, scratch);
        let writer = b.load(response, 40, I32);
        let status = b.load(response, 44, I32);
        let event = offset(b, scratch, 192);
        b.call(self.finish_write, &[writer, status, event], &[]);
        b.call(self.native["drop-trailers-writer"], &[writer], &[]);
        error
    }
}

fn emit_fetch(
    module: &mut Module<'static>,
    memory: Memory,
    function: Func,
    t: &Transport<'_>,
    strings: StringHelperFuncs,
    normalize: Func,
    bytes: ByteHelpers,
) -> Result<()> {
    let mut b = Builder::new(module, function, memory);
    let url = b.param(0);
    let zero = b.integer(0);
    let one = b.integer(1);
    let code = b.number(12.0);
    let count = b.integer(8);
    let frame = b.call(t.allocator.frame_new, &[count], &[I32])[0];
    b.store(frame, 12, url, I32);
    b.store(frame, 28, b.param(1), I32);
    b.store(frame, 32, b.param(2), I32);
    b.store(frame, 36, b.param(3), I32);
    let response = b.allocate(t.allocator.realloc, 64, 4);
    b.store(frame, 16, response, I32);
    let size = b.integer(64);
    b.effect(O::MemoryFill { mem: memory }, &[response, zero, size]);
    let backlink = b.integer(4);
    let backlink = b.op(O::I32Sub, &[response, backlink], I32);
    let header = b.load(backlink, 0, I32);
    let kind = b.integer(14);
    b.store(header, 16, kind, I32);
    b.store(response, 56, frame, I32);
    let scratch = b.allocate(t.allocator.realloc, 256, 8);
    b.store(frame, 20, scratch, I32);
    b.store(response, 48, scratch, I32);
    let size = b.integer(256);
    b.effect(O::MemoryFill { mem: memory }, &[scratch, zero, size]);
    let data = b.load(url, 0, I32);
    let length = b.load(url, 4, I32);
    let max = b.integer((u32::MAX - 64) / 3);
    let fits = b.op(O::I32LeU, &[length, max], I32);
    b.require(fits);
    let three = b.integer(3);
    let capacity = b.op(O::I32Mul, &[length, three], I32);
    let capacity = offset(&mut b, capacity, 64);
    let normalized = b.call(t.allocator.realloc, &[zero, zero, one, capacity], &[I32])[0];
    b.store(frame, 24, normalized, I32);
    let invalid = b.call(normalize, &[data, length, normalized, capacity], &[I32])[0];
    let invalid_url = b.body.add_block();
    let valid_url = b.body.add_block();
    b.branch(invalid, invalid_url, valid_url);
    b.block = invalid_url;
    b.call(t.allocator.frame_drop, &[frame], &[]);
    b.ret(&[one, code]);
    b.block = valid_url;
    let normalized_data = offset(&mut b, normalized, 32);
    let length = b.load(normalized, 16, I32);
    let url = b.call(strings.lift_canonical, &[normalized_data, length], &[I32])[0];
    b.store(response, 16, url, I32);
    let method = request::method(&mut b, t, frame)?;
    request::fields(&mut b, t, frame, scratch, response)?;
    let fields = b.load(scratch, 4, I32);
    let has_body = b.op(O::I32Ne, &[b.param(4), zero], I32);
    let stream = b.body.add_block();
    let empty_body = b.body.add_block();
    let request_body = b.body.add_block();
    let body_reader = b.body.add_blockparam(request_body, I32);
    let body_writer = b.body.add_blockparam(request_body, I32);
    b.branch(has_body, stream, empty_body);
    b.block = stream;
    let pair = b.call(t.native["new-stream"], &[], &[I64])[0];
    let reader = low(&mut b, pair);
    let writer = high(&mut b, pair);
    b.jump(request_body, &[reader, writer]);
    b.block = empty_body;
    b.jump(request_body, &[zero, zero]);
    b.block = request_body;
    let trailers = b.call(t.native["new-trailers"], &[], &[I64])[0];
    let reader = low(&mut b, trailers);
    let writer = high(&mut b, trailers);
    b.store(response, 40, writer, I32);
    b.call(
        t.native["request"],
        &[fields, has_body, body_reader, reader, zero, zero, scratch],
        &[],
    );
    let request = b.load(scratch, 0, I32);
    let done = b.load(scratch, 4, I32);
    b.store(response, 36, done, I32);
    let method_error = b.call(
        t.native["set-method"],
        &[request, method[0], method[1], method[2]],
        &[I32],
    )[0];
    let scheme = b.load(normalized, 0, I32);
    let scheme_error = b.call(
        t.native["scheme"],
        &[request, one, scheme, zero, zero],
        &[I32],
    )[0];
    let start = b.load(normalized, 4, I32);
    let end = b.load(normalized, 8, I32);
    let authority = b.op(O::I32Add, &[normalized_data, start], I32);
    let length = b.op(O::I32Sub, &[end, start], I32);
    let authority_error = b.call(
        t.native["authority"],
        &[request, one, authority, length],
        &[I32],
    )[0];
    let start = b.load(normalized, 12, I32);
    let end = b.load(normalized, 16, I32);
    let path = b.op(O::I32Add, &[normalized_data, start], I32);
    let length = b.op(O::I32Sub, &[end, start], I32);
    let path_error = b.call(t.native["path"], &[request, one, path, length], &[I32])[0];
    let errors = b.op(O::I32Or, &[scheme_error, authority_error], I32);
    let errors = b.op(O::I32Or, &[errors, path_error], I32);
    let errors = b.op(O::I32Or, &[errors, method_error], I32);
    let rejected = b.body.add_block();
    let send = b.body.add_block();
    b.branch(errors, rejected, send);
    b.block = rejected;
    b.call(t.native["drop-request"], &[request], &[]);
    let close_body = b.body.add_block();
    let cleanup = b.body.add_block();
    b.branch(has_body, close_body, cleanup);
    b.block = close_body;
    b.call(t.native["drop-writer"], &[body_writer], &[]);
    b.jump(cleanup, &[]);
    b.block = cleanup;
    b.call(t.native["drop-completion-reader"], &[done], &[]);
    let empty = offset(&mut b, scratch, 64);
    let status = b.call(t.native["write-trailers"], &[writer, empty], &[I32])[0];
    let event = offset(&mut b, scratch, 192);
    b.call(t.finish_write, &[writer, status, event], &[]);
    b.call(t.native["drop-trailers-writer"], &[writer], &[]);
    b.call(t.allocator.frame_drop, &[frame], &[]);
    b.ret(&[one, code]);
    b.block = send;
    request::start_upload(&mut b, t, bytes, response, frame, body_writer, has_body);
    let empty = offset(&mut b, scratch, 64);
    let status = b.call(t.native["write-trailers"], &[writer, empty], &[I32])[0];
    b.store(response, 44, status, I32);
    b.call(t.native["send"], &[request, scratch], &[]);
    let error = error_code(&mut b, scratch);
    let failed = b.body.add_block();
    let received = b.body.add_block();
    b.branch(error, failed, received);
    b.block = failed;
    t.finish_request(&mut b, response, scratch);
    b.call(t.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[error], F64);
    b.ret(&[one, payload]);
    b.block = received;
    let handle = b.load(scratch, 8, I32);
    let status = b.call(t.native["status"], &[handle], &[I32])[0];
    b.store(response, 0, status, I32);
    let headers = b.call(t.native["headers"], &[handle], &[I32])[0];
    let out = offset(&mut b, response, 4);
    b.call(t.native["copy-fields"], &[headers, out], &[]);
    b.call(t.native["drop-fields"], &[headers], &[]);
    let ack = b.call(t.native["new-completion"], &[], &[I64])[0];
    let reader = low(&mut b, ack);
    let writer = high(&mut b, ack);
    b.store(response, 32, writer, I32);
    b.call(t.native["consume"], &[handle, reader, scratch], &[]);
    let stream = b.load(scratch, 0, I32);
    let trailers = b.load(scratch, 4, I32);
    b.store(response, 24, stream, I32);
    b.store(response, 28, trailers, I32);
    let address = b.integer(OWNERS);
    let count = b.load(address, 0, I32);
    let count = b.op(O::I32Add, &[count, one], I32);
    b.store(address, 0, count, I32);
    let head = b.op(O::I32Eq, &[method[0], one], I32);
    let mut null_body = head;
    for code in [204, 205, 304] {
        let code = b.integer(code);
        let matches = b.op(O::I32Eq, &[status, code], I32);
        null_body = b.op(O::I32Or, &[null_body, matches], I32);
    }
    let empty = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(null_body, empty, ready);
    b.block = empty;
    let consumed = b.call(t.consume, &[response], &[I32, F64]);
    let failed = b.body.add_block();
    let consumed_ok = b.body.add_block();
    b.branch(consumed[0], failed, consumed_ok);
    b.block = failed;
    b.ret(&consumed);
    b.block = consumed_ok;
    b.store(response, 20, zero, I32);
    b.store(response, 52, one, I32);
    b.jump(ready, &[]);
    b.block = ready;
    let payload = b.op(O::F64ConvertI32U, &[response], F64);
    b.ret(&[zero, payload]);
    b.finish(module, function)
}

fn emit_body(
    module: &mut Module<'static>,
    memory: Memory,
    function: Func,
    t: &Transport<'_>,
    bytes: ByteHelpers,
    buffered: Func,
) -> Result<()> {
    let mut b = Builder::new(module, function, memory);
    let response = b.param(0);
    let zero = b.integer(0);
    let one = b.integer(1);
    let null_body = b.load(response, 52, I32);
    let empty = b.body.add_block();
    let consume = b.body.add_block();
    b.branch(null_body, empty, consume);
    b.block = empty;
    let empty_bytes = b.load(response, 12, I32);
    let empty_bytes = b.call(bytes.copy, &[empty_bytes], &[I32])[0];
    let payload = b.op(O::F64ConvertI32U, &[empty_bytes], F64);
    b.ret(&[zero, payload]);
    b.block = consume;
    let used = b.load(response, 20, I32);
    reject(&mut b, used, 12);
    b.store(response, 20, one, I32);
    let scratch = b.load(response, 48, I32);
    let frame = b.load(response, 56, I32);
    let stream = b.load(response, 24, I32);
    let limit = b.integer(u32::MAX);
    let result = b.call(buffered, &[stream, limit], &[I32, I32, I32]);
    b.store(frame, 24, result[1], I32);
    b.call(t.native["drop-reader"], &[stream], &[]);
    let trailers = b.load(response, 28, I32);
    let status = b.call(t.native["read-trailers"], &[trailers, scratch], &[I32])[0];
    let done = b.op(O::I32Eqz, &[status], I32);
    b.require(done);
    b.call(t.native["drop-trailers-reader"], &[trailers], &[]);
    let error = error_code(&mut b, scratch);
    let has_error = b.body.add_block();
    let success = b.body.add_block();
    let acknowledged = b.body.add_block();
    b.branch(error, has_error, success);
    b.block = has_error;
    b.store(scratch, 128, one, I32);
    let internal_error = b.integer(25);
    b.store(scratch, 136, internal_error, I32);
    b.jump(acknowledged, &[]);
    b.block = success;
    let present = byte(&mut b, scratch, 8);
    let fields = b.body.add_block();
    b.branch(present, fields, acknowledged);
    b.block = fields;
    let field = b.load(scratch, 12, I32);
    b.call(t.native["drop-fields"], &[field], &[]);
    b.jump(acknowledged, &[]);
    b.block = acknowledged;
    let writer = b.load(response, 32, I32);
    let acknowledgement = offset(&mut b, scratch, 128);
    let status = b.call(
        t.native["write-completion"],
        &[writer, acknowledgement],
        &[I32],
    )[0];
    let event = offset(&mut b, scratch, 192);
    b.call(t.finish_write, &[writer, status, event], &[]);
    b.call(t.native["drop-completion-writer"], &[writer], &[]);
    let request_error = t.finish_request(&mut b, response, scratch);
    let error = b.op(O::Select, &[error, request_error, error], I32);
    let address = b.integer(OWNERS);
    let count = b.load(address, 0, I32);
    let count = b.op(O::I32Sub, &[count, one], I32);
    b.store(address, 0, count, I32);
    let failed = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(error, failed, ready);
    b.block = failed;
    b.call(t.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[error], F64);
    b.ret(&[one, payload]);
    b.block = ready;
    let view = b.call(bytes.lift_canonical, &[result[1], result[2]], &[I32])[0];
    b.store(response, 12, view, I32);
    b.call(t.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[view], F64);
    b.ret(&[zero, payload]);
    b.finish(module, function)
}

#[cfg(test)]
#[path = "../../helpers/fetch.rs"]
mod algorithms;

pub(crate) fn declare_helpers(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    let mut functions = imports::declare_imports(
        module,
        crate::waffle_backend::link::HELPER_MODULE,
        &[
            imports::Function {
                name: "fetch_url".into(),
                params: vec!["i32"; 4],
                results: vec!["i32"],
            },
            imports::Function {
                name: "fetch_header".into(),
                params: vec!["i32"; 2],
                results: vec!["i32"],
            },
            imports::Function {
                name: "fetch_method".into(),
                params: vec!["i32"; 2],
                results: vec!["i32"],
            },
            imports::Function {
                name: "fetch_header_size".into(),
                params: vec!["i32"; 4],
                results: vec!["i32"],
            },
            imports::Function {
                name: "fetch_header_get".into(),
                params: vec!["i32"; 6],
                results: vec!["i32"],
            },
            imports::Function {
                name: "fetch_header_value".into(),
                params: vec!["i32"; 4],
                results: vec!["i32"],
            },
            imports::Function {
                name: "fetch_decode".into(),
                params: vec!["i32"; 4],
                results: vec!["i32"],
            },
        ],
    );
    functions.extend(imports::declare_imports(
        module,
        "http",
        &[
            imports::Function {
                name: "set-method".into(),
                params: vec!["i32"; 4],
                results: vec!["i32"],
            },
            imports::Function {
                name: "new-stream".into(),
                params: vec![],
                results: vec!["i64"],
            },
            imports::Function {
                name: "write".into(),
                params: vec!["i32"; 3],
                results: vec!["i32"],
            },
            imports::Function {
                name: "drop-writer".into(),
                params: vec!["i32"],
                results: vec![],
            },
        ],
    ));
    functions
}
