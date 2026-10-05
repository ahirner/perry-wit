//! Standard Response values share metadata and body ownership with fetched responses.
use crate::waffle_backend::{
    allocation::AllocationFuncs,
    bytes::ByteHelpers,
    runtime::{
        builder::{self, Builder},
        imports,
    },
    strings::StringPool,
};
use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
    Value,
};

pub(crate) const HEADERS: u32 = 68;
pub(crate) const NATIVE: u32 = 72;
pub(crate) const STATUS_TEXT: u32 = 76;
pub(crate) const SIGNAL: u32 = 88;
pub(crate) const READER_DROPPED: u32 = 92;
pub(crate) const SIZE: u32 = 96;

pub(crate) struct Runtime<'a> {
    pub(crate) allocator: AllocationFuncs,
    pub(crate) bytes: ByteHelpers,
    pub(crate) headers: super::headers::Helpers,
    pub(crate) validate_text: Func,
    pub(crate) pool: &'a StringPool,
}
fn reject(b: &mut Builder, allocator: AllocationFuncs, frame: Value, invalid: Value) {
    let failed = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(invalid, failed, ready);
    b.block = failed;
    b.call(allocator.frame_drop, &[frame], &[]);
    let one = b.integer(1);
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = ready;
}
pub(crate) fn declare(module: &mut Module<'static>) -> Func {
    imports::declare_imports(
        module,
        crate::waffle_backend::link::HELPER_MODULE,
        &[imports::Function {
            name: "fetch_status_text".into(),
            params: vec!["i32"; 2],
            results: vec!["i32"],
        }],
    )["fetch_status_text"]
}
pub(crate) fn emit(module: &mut Module<'static>, memory: Memory, r: &Runtime<'_>) -> Result<Func> {
    // body, body-kind, optional status entry, headers, header-mode, status-text.
    let function = builder::declare(module, "response.new", &[I32; 6], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let zero = b.integer(0);
    let count = b.integer(6);
    let frame = b.call(r.allocator.frame_new, &[count], &[I32])[0];
    for (at, index) in [(12, 0), (16, 2), (20, 3), (24, 5)] {
        b.store(frame, at, b.param(index), I32);
    }
    let default = b.body.add_block();
    let field = b.body.add_block();
    let supplied = b.body.add_block();
    let ready_status = b.body.add_block();
    let status = b.body.add_blockparam(ready_status, I32);
    b.branch(b.param(2), field, default);
    b.block = field;
    let tag = b.load(b.param(2), 8, I32);
    b.branch(tag, supplied, default);
    b.block = default;
    let normal = b.integer(200);
    b.jump(ready_status, &[normal]);
    b.block = supplied;
    let value = b.load(b.param(2), 16, F64);
    let value = b.op(O::F64Trunc, &[value], F64);
    let modulus = b.number(65536.0);
    let quotient = b.op(O::F64Div, &[value, modulus], F64);
    let quotient = b.op(O::F64Floor, &[quotient], F64);
    let multiple = b.op(O::F64Mul, &[quotient, modulus], F64);
    let value = b.op(O::F64Sub, &[value, multiple], F64);
    let low = b.number(200.0);
    let high = b.number(599.0);
    let lower = b.op(O::F64Ge, &[value, low], I32);
    let upper = b.op(O::F64Le, &[value, high], I32);
    let valid = b.op(O::I32And, &[lower, upper], I32);
    let invalid = b.op(O::I32Eqz, &[valid], I32);
    reject(&mut b, r.allocator, frame, invalid);
    let value = b.op(O::I32TruncF64U, &[value], I32);
    b.jump(ready_status, &[value]);
    b.block = ready_status;
    let mut prohibited = zero;
    for code in [204, 205, 304] {
        let code = b.integer(code);
        let matches = b.op(O::I32Eq, &[status, code], I32);
        prohibited = b.op(O::I32Or, &[prohibited, matches], I32);
    }
    let has_body = b.op(O::I32Ne, &[b.param(1), zero], I32);
    let prohibited = b.op(O::I32And, &[prohibited, has_body], I32);
    reject(&mut b, r.allocator, frame, prohibited);
    let empty = b.integer(r.pool.get("").unwrap());
    let status_text = b.op(O::Select, &[b.param(5), empty, b.param(5)], I32);
    let data = b.load(status_text, 0, I32);
    let length = b.load(status_text, 4, I32);
    let invalid = b.call(r.validate_text, &[data, length], &[I32])[0];
    reject(&mut b, r.allocator, frame, invalid);
    let response = b.allocate(r.allocator.realloc, SIZE, 4);
    b.store(frame, 28, response, I32);
    let size = b.integer(SIZE);
    b.effect(O::MemoryFill { mem: memory }, &[response, zero, size]);
    let four = b.integer(4);
    let backlink = b.op(O::I32Sub, &[response, four], I32);
    let header = b.load(backlink, 0, I32);
    let kind = b.integer(14);
    b.store(header, 16, kind, I32);
    b.store(response, 0, status, I32);
    b.store(response, 16, empty, I32);
    b.store(response, STATUS_TEXT, status_text, I32);
    let null = b.op(O::I32Eqz, &[has_body], I32);
    b.store(response, 52, null, I32);
    let fields = b.call(r.headers.new, &[b.param(4), b.param(3)], &[I32, F64]);
    reject(&mut b, r.allocator, frame, fields[0]);
    let fields = b.op(O::I32TruncF64U, &[fields[1]], I32);
    b.store(response, HEADERS, fields, I32);
    let name = b.integer(r.pool.get("content-type").unwrap());
    let value = b.integer(r.pool.get("text/plain;charset=UTF-8").unwrap());
    let result = b.call(
        r.headers.default_type,
        &[fields, b.param(1), name, value],
        &[I32, F64],
    );
    reject(&mut b, r.allocator, frame, result[0]);
    let copy = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(has_body, copy, ready);
    b.block = copy;
    let body = b.call(r.bytes.copy, &[b.param(0)], &[I32])[0];
    b.store(response, 12, body, I32);
    b.jump(ready, &[]);
    b.block = ready;
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[response], F64);
    b.ret(&[zero, payload]);
    b.finish(module, function)?;
    Ok(function)
}
