//! Headers-first fetch and separately owned body consumption over official P3 HTTP.

use crate::waffle_backend::{
    allocation::AllocationFuncs,
    runtime::{
        builder::{self, Builder},
        imports,
    },
    streams,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32, I64},
    Value,
};

mod owners;
mod redirects;
mod upload;

pub(crate) const RESPONSE_TYPE: &str = "__perry_fetch_response";
pub(crate) const OWNERS: u32 = 104;

pub(crate) fn is_response(ty: &perry_hir::types::Type) -> bool {
    matches!(ty, perry_hir::types::Type::Named(name) if name == RESPONSE_TYPE)
}

#[derive(Clone, Copy)]
pub(crate) struct Helpers {
    pub(crate) stream: Option<streams::web::NativeBody>,
    pub(crate) fetch: Func,
    pub(crate) upload: Func,
    pub(crate) finish: Func,
    pub(crate) cancel_unused: Option<Func>,
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

pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    runtime: &super::FetchRuntime<'_>,
) -> Result<Helpers> {
    let allocator = runtime.allocator;
    let strings = runtime.strings;
    let native = runtime.imports;
    let promises = runtime.promises.unwrap();
    let fetch = builder::declare(module, "fetch", &[I32; 9], &[I32, F64]);
    let send = builder::declare(module, "fetch.send", &[I32], &[I32, F64]);
    let release = builder::declare(module, "fetch.release", &[I32], &[I32]);
    let discard = builder::declare(module, "fetch.discard", &[I32], &[I32, F64]);
    let finish = builder::declare(module, "fetch.finish", &[], &[]);
    let transport = Transport {
        native,
        pool: runtime.pool,
        headers: runtime.headers.unwrap(),
        allocator,
        finish_write: super::future::emit_finish_write(module, memory, native)?,
        promises,
        release,
        operations: runtime.operations,
        abort: runtime.abort.unwrap(),
    };
    emit_fetch(module, memory, send, &transport, strings)?;
    let upload = upload::emit(module, memory, native, runtime.operations)?;
    emit_release(module, memory, release, &transport)?;
    let cancel_unused = owners::emit_cancel_unused(module, memory, &transport, release)?;
    owners::emit_abort_notify(module, memory, &transport)?;
    redirects::emit_discard(module, memory, discard, &transport, release)?;
    redirects::emit(module, memory, fetch, send, discard, &transport, runtime)?;
    let mut b = Builder::new(module, finish, memory);
    if let Some(cleanup) = cancel_unused {
        let closing = b.integer(1);
        b.call(cleanup, &[closing], &[]);
    }
    let address = b.integer(OWNERS);
    let pending = b.load(address, 0, I32);
    let clear = b.op(O::I32Eqz, &[pending], I32);
    b.require(clear);
    b.ret(&[]);
    b.finish(module, finish)?;
    Ok(Helpers {
        stream: runtime
            .operations
            .map(|operations| streams::web::NativeBody {
                read: native["async-read"],
                release,
                operations,
            }),
        fetch,
        upload,
        finish,
        cancel_unused,
    })
}

struct Transport<'a> {
    pool: &'a crate::waffle_backend::strings::StringPool,
    native: &'a BTreeMap<String, Func>,
    headers: super::headers::Helpers,
    allocator: AllocationFuncs,
    finish_write: Func,
    release: Func,
    promises: &'a crate::waffle_backend::registry::PromiseImports,
    operations: Option<crate::waffle_backend::runtime::operations::Operations>,
    abort: crate::waffle_backend::abort::Helpers,
}
impl Transport<'_> {
    fn finish_request(&self, b: &mut Builder, response: Value, scratch: Value) -> Value {
        let upload = b.load(response, 60, I32);
        let join = b.body.add_block();
        let complete = b.body.add_block();
        b.branch(upload, join, complete);
        b.block = join;
        b.call(self.promises.await_native, &[upload], &[I32, F64]);
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
    strings: crate::waffle_backend::strings::StringHelperFuncs,
) -> Result<()> {
    let mut b = Builder::new(module, function, memory);
    let request_value = b.param(0);
    let signal = b.load(request_value, super::request::SIGNAL, I32);
    let signal = b.call(t.abort.root, &[signal], &[I32])[0];
    if let Some(operations) = t.operations {
        b.call(operations.bind_signal, &[signal], &[]);
        let aborted = b.call(operations.aborted, &[], &[I32])[0];
        reject(&mut b, aborted, 20);
    }
    let zero = b.integer(0);
    let one = b.integer(1);
    let code = b.number(12.0);
    let count = b.integer(8);
    let frame = b.call(t.allocator.frame_new, &[count], &[I32])[0];
    b.store(frame, 12, request_value, I32);
    let response = b.allocate(t.allocator.realloc, super::response::SIZE, 4);
    b.store(frame, 16, response, I32);
    let size = b.integer(super::response::SIZE);
    b.effect(O::MemoryFill { mem: memory }, &[response, zero, size]);
    let backlink = b.integer(4);
    let backlink = b.op(O::I32Sub, &[response, backlink], I32);
    let header = b.load(backlink, 0, I32);
    let kind = b.integer(14);
    b.store(header, 16, kind, I32);
    b.store(response, 56, frame, I32);
    b.store(response, super::response::SIGNAL, signal, I32);
    b.store(response, super::response::NATIVE, one, I32);
    let empty = b.integer(t.pool.get("").unwrap());
    b.store(response, super::response::STATUS_TEXT, empty, I32);
    let scratch = b.allocate(t.allocator.realloc, 256, 8);
    b.store(frame, 20, scratch, I32);
    b.store(response, 48, scratch, I32);
    let size = b.integer(256);
    b.effect(O::MemoryFill { mem: memory }, &[scratch, zero, size]);
    let normalized = b.load(request_value, super::request::URL_PARTS, I32);
    let normalized_data = offset(&mut b, normalized, 32);
    let url_length = b.load(normalized, 16, I32);
    let url = b.call(
        strings.lift_canonical,
        &[normalized_data, url_length],
        &[I32],
    )[0];
    b.store(response, 16, url, I32);
    let method_tag = b.load(request_value, super::request::METHOD_TAG, I32);
    let method_text = b.load(request_value, super::request::METHOD, I32);
    let method = [
        method_tag,
        b.load(method_text, 0, I32),
        b.load(method_text, 4, I32),
    ];
    let fields_data = b.load(request_value, 4, I32);
    let fields_count = b.load(request_value, 8, I32);
    b.call(
        t.native["fields"],
        &[fields_data, fields_count, scratch],
        &[],
    );
    let failed = byte(&mut b, scratch, 0);
    let invalid_fields = b.body.add_block();
    let ready_fields = b.body.add_block();
    b.branch(failed, invalid_fields, ready_fields);
    b.block = invalid_fields;
    b.call(t.allocator.frame_drop, &[frame], &[]);
    b.ret(&[one, code]);
    b.block = ready_fields;
    let fields = b.load(scratch, 4, I32);
    let body_kind = b.load(request_value, super::request::BODY_KIND, I32);
    let has_body = b.op(O::I32Ne, &[body_kind, zero], I32);
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
    let upload_body = b.load(request_value, super::request::BODY, I32);
    upload::start(&mut b, t, response, body_writer, has_body, upload_body);
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
    let error = if let Some(operations) = t.operations {
        let aborted = b.call(operations.aborted, &[], &[I32])[0];
        let code = b.integer(20);
        b.op(O::Select, &[code, error, aborted], I32)
    } else {
        error
    };
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
    let data = b.load(response, 4, I32);
    let count = b.load(response, 8, I32);
    let fields = b.call(t.headers.wrap, &[data, count, one], &[I32])[0];
    b.store(response, super::response::HEADERS, fields, I32);
    let ack = b.call(t.native["new-completion"], &[], &[I64])[0];
    let reader = low(&mut b, ack);
    let writer = high(&mut b, ack);
    b.store(response, 32, writer, I32);
    b.call(t.native["consume"], &[handle, reader, scratch], &[]);
    let stream = b.load(scratch, 0, I32);
    let trailers = b.load(scratch, 4, I32);
    b.store(response, 24, stream, I32);
    b.store(response, 28, trailers, I32);
    owners::retain(&mut b, response);
    if let Some(operations) = t.operations {
        let cancelled = b.call(operations.aborted, &[], &[I32])[0];
        let cleanup = b.body.add_block();
        let ready = b.body.add_block();
        b.branch(cancelled, cleanup, ready);
        b.block = cleanup;
        b.store(response, 20, one, I32);
        b.call(t.release, &[response], &[I32]);
        b.call(t.allocator.frame_drop, &[frame], &[]);
        let error = b.number(20.0);
        b.ret(&[one, error]);
        b.block = ready;
    }
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
    let error = b.call(t.release, &[response], &[I32])[0];
    b.call(t.allocator.frame_drop, &[frame], &[]);
    let failed = b.body.add_block();
    let consumed_ok = b.body.add_block();
    b.branch(error, failed, consumed_ok);
    b.block = failed;
    let payload = b.op(O::F64ConvertI32U, &[error], F64);
    b.ret(&[one, payload]);
    b.block = consumed_ok;
    b.store(response, 20, zero, I32);
    b.store(response, 52, one, I32);
    b.jump(ready, &[]);
    b.block = ready;
    let payload = b.op(O::F64ConvertI32U, &[response], F64);
    b.ret(&[zero, payload]);
    b.finish(module, function)
}

fn emit_release(
    module: &mut Module<'static>,
    memory: Memory,
    function: Func,
    t: &Transport<'_>,
) -> Result<()> {
    let mut b = Builder::new(module, function, memory);
    let response = b.param(0);
    let scratch = b.load(response, 48, I32);
    let stream = b.load(response, 24, I32);
    let one = b.integer(1);
    if let Some(operations) = t.operations {
        let signal = b.load(response, super::response::SIGNAL, I32);
        b.call(operations.bind_signal, &[signal], &[]);
    }
    owners::drop_reader(&mut b, t, response, stream);
    let trailers = b.load(response, 28, I32);
    let status = b.call(t.native["read-trailers"], &[trailers, scratch], &[I32])[0];
    let done = b.op(O::I32Eqz, &[status], I32);
    b.require(done);
    b.call(t.native["drop-trailers-reader"], &[trailers], &[]);
    let mut error = error_code(&mut b, scratch);
    if let Some(operations) = t.operations {
        let aborted = b.call(operations.aborted, &[], &[I32])[0];
        let code = b.integer(20);
        error = b.op(O::Select, &[code, error, aborted], I32);
    }
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
    owners::release(&mut b, response);
    b.ret(&[error]);
    b.finish(module, function)
}

#[cfg(test)]
#[path = "../../helpers/fetch.rs"]
mod algorithms;

pub(crate) fn declare_helpers(module: &mut Module<'static>, owned: bool) -> BTreeMap<String, Func> {
    let mut functions = imports::declare_imports(
        module,
        crate::waffle_backend::link::HELPER_MODULE,
        &[imports::Function {
            name: "fetch_redirect".into(),
            params: vec!["i32"; 6],
            results: vec!["i32"],
        }],
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
                name: "drop-writer".into(),
                params: vec!["i32"],
                results: vec![],
            },
        ],
    ));
    if !owned {
        functions.extend(imports::declare_imports(
            module,
            "http",
            &[imports::Function {
                name: "write".into(),
                params: vec!["i32"; 3],
                results: vec!["i32"],
            }],
        ));
    }
    functions
}
