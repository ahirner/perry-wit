//! Shared single-consumption byte, UTF-8, and JSON body methods.
use crate::waffle_backend::{
    allocation::AllocationFuncs,
    json::JsonHelpers,
    runtime::builder::{self, Builder},
    strings::StringHelperFuncs,
};
use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as O, Terminator,
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
    pub(crate) response: Option<Func>,
}
pub(crate) fn emit(module: &mut Module<'static>, memory: Memory, r: &Runtime) -> Result<Helpers> {
    let allocator = r.allocator;
    let strings = r.strings;
    let body = builder::declare(module, "body.bytes", &[I32; 2], &[I32, F64]);
    let text = builder::declare(module, "body.text", &[I32; 2], &[I32, F64]);
    let mut b = Builder::new(module, body, memory);
    let kind = b.param(1);
    let one = b.integer(1);
    let valid = b.op(O::I32LeU, &[kind, one], I32);
    b.require(valid);
    let receiver = b.param(0);
    let response = b.body.add_block();
    let request = b.body.add_block();
    let buffered = b.body.add_block();
    let present = b.body.add_blockparam(buffered, I32);
    let used_address = b.body.add_blockparam(buffered, I32);
    b.branch(kind, response, request);
    b.block = request;
    let has_body = b.load(receiver, super::request::BODY_KIND, I32);
    let used_offset = b.integer(super::request::BODY_USED);
    let used = b.op(O::I32Add, &[receiver, used_offset], I32);
    b.jump(buffered, &[has_body, used]);
    b.block = response;
    let native = b.load(receiver, super::response::NATIVE, I32);
    let transport = b.body.add_block();
    let constructed = b.body.add_block();
    b.branch(native, transport, constructed);
    b.block = transport;
    if let Some(consume) = r.response {
        let result = b.call(consume, &[receiver], &[I32, F64]);
        b.ret(&result);
    } else {
        b.body.set_terminator(b.block, Terminator::Unreachable);
    }
    b.block = constructed;
    let null_body = b.load(receiver, 52, I32);
    let has_body = b.op(O::I32Eqz, &[null_body], I32);
    let used_offset = b.integer(20);
    let used = b.op(O::I32Add, &[receiver, used_offset], I32);
    b.jump(buffered, &[has_body, used]);
    b.block = buffered;
    let content = b.body.add_block();
    let empty = b.body.add_block();
    b.branch(present, content, empty);
    b.block = empty;
    let length = b.number(0.0);
    let bytes = b.call(r.bytes.new, &[length], &[I32, F64]);
    b.ret(&bytes);
    b.block = content;
    let used = b.load(used_address, 0, I32);
    let fail = b.body.add_block();
    let copy = b.body.add_block();
    b.branch(used, fail, copy);
    b.block = fail;
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = copy;
    b.store(used_address, 0, one, I32);
    let source = b.load(receiver, 12, I32);
    let bytes = b.call(r.bytes.copy, &[source], &[I32])[0];
    let payload = b.op(O::F64ConvertI32U, &[bytes], F64);
    let zero = b.integer(0);
    b.ret(&[zero, payload]);
    b.finish(module, body)?;
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
