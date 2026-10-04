//! Owned standard header results from the allocation-free canonical field codec.

use super::SourceRuntime;
use crate::waffle_backend::runtime::builder::{self, Builder};
use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
};

pub(crate) const HEADERS_TYPE: &str = "__perry_fetch_headers";
pub(crate) fn is_headers(ty: &perry_hir::types::Type) -> bool {
    matches!(ty, perry_hir::types::Type::Named(name) if name == HEADERS_TYPE)
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    runtime: &SourceRuntime<'_>,
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
