//! Standard Request values own validated metadata and immutable upload snapshots.
use crate::waffle_backend::{
    allocation::AllocationFuncs,
    bytes::ByteHelpers,
    runtime::{
        builder::{self, Builder},
        imports,
    },
    strings::{StringHelperFuncs, StringPool},
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
    Value,
};

pub(crate) const REQUEST_TYPE: &str = "__perry_fetch_request";
pub(crate) const BODY: u32 = 12;
pub(crate) const URL: u32 = 16;
pub(crate) const METHOD: u32 = 20;
pub(crate) const REDIRECT: u32 = 24;
pub(crate) const BODY_KIND: u32 = 28;
pub(crate) const BODY_USED: u32 = 32;
pub(crate) const METHOD_TAG: u32 = 36;
pub(crate) const URL_PARTS: u32 = 40;
pub(crate) const REDIRECT_MODE: u32 = 44;
pub(crate) fn is_request(ty: &perry_hir::types::Type) -> bool {
    matches!(ty, perry_hir::types::Type::Named(name) if name == REQUEST_TYPE)
}
#[derive(Clone, Copy)]
pub(crate) struct Helpers {
    pub(crate) new: Func,
}
pub(crate) struct Runtime<'a> {
    pub(crate) allocator: AllocationFuncs,
    pub(crate) bytes: ByteHelpers,
    pub(crate) strings: StringHelperFuncs,
    pub(crate) headers: super::headers::Helpers,
    pub(crate) imports: &'a BTreeMap<String, Func>,
    pub(crate) pool: &'a StringPool,
}
pub(crate) fn declare_helpers(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    imports::declare_imports(
        module,
        crate::waffle_backend::link::HELPER_MODULE,
        &[
            imports::Function {
                name: "fetch_url".into(),
                params: vec!["i32"; 4],
                results: vec!["i32"],
            },
            imports::Function {
                name: "fetch_method".into(),
                params: vec!["i32"; 2],
                results: vec!["i32"],
            },
        ],
    )
}
fn reject(b: &mut Builder, r: &Runtime<'_>, frame: Value, invalid: Value) {
    let failed = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(invalid, failed, ready);
    b.block = failed;
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let one = b.integer(1);
    let error = b.number(12.0);
    b.ret(&[one, error]);
    b.block = ready;
}
pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
) -> Result<Helpers> {
    // input, is-Request, method, headers, header-mode, body, body-kind, redirect.
    let function = builder::declare(module, "request.new", &[I32; 8], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let zero = b.integer(0);
    let one = b.integer(1);
    let three = b.integer(3);
    let count = b.integer(8);
    let frame = b.call(r.allocator.frame_new, &[count], &[I32])[0];
    for (at, index) in [(12, 0), (16, 2), (20, 3), (24, 5), (28, 7)] {
        b.store(frame, at, b.param(index), I32);
    }
    let input = b.param(0);
    let source = b.body.add_block();
    let url_input = b.body.add_block();
    let defaults = b.body.add_block();
    let [
        url,
        inherited_method,
        inherited_headers,
        inherited_body,
        inherited_kind,
        inherited_redirect,
    ] = [(); 6].map(|_| b.body.add_blockparam(defaults, I32));
    b.branch(b.param(1), source, url_input);
    b.block = url_input;
    let get = b.integer(r.pool.get("GET").unwrap());
    let follow = b.integer(r.pool.get("follow").unwrap());
    b.jump(defaults, &[input, get, zero, zero, zero, follow]);
    b.block = source;
    let source_url = b.load(input, URL, I32);
    let source_method = b.load(input, METHOD, I32);
    let source_body = b.load(input, BODY, I32);
    let source_kind = b.load(input, BODY_KIND, I32);
    let source_redirect = b.load(input, REDIRECT, I32);
    b.jump(
        defaults,
        &[
            source_url,
            source_method,
            input,
            source_body,
            source_kind,
            source_redirect,
        ],
    );
    b.block = defaults;
    let method = b.op(O::Select, &[b.param(2), inherited_method, b.param(2)], I32);
    let headers = b.op(O::Select, &[b.param(3), inherited_headers, b.param(3)], I32);
    let inherited_header_mode = b.op(O::Select, &[three, zero, b.param(1)], I32);
    let header_mode = b.op(
        O::Select,
        &[b.param(4), inherited_header_mode, b.param(3)],
        I32,
    );
    let body = b.op(O::Select, &[b.param(5), inherited_body, b.param(6)], I32);
    let body_kind = b.op(O::Select, &[b.param(6), inherited_kind, b.param(6)], I32);
    let redirect = b.op(
        O::Select,
        &[b.param(7), inherited_redirect, b.param(7)],
        I32,
    );
    for (at, value) in [(16, method), (20, headers), (24, body), (28, redirect)] {
        b.store(frame, at, value, I32);
    }
    let inherited = b.op(O::I32Eqz, &[b.param(6)], I32);
    let inherited = b.op(O::I32And, &[inherited, b.param(1)], I32);
    let has_body = b.op(O::I32Ne, &[body_kind, zero], I32);
    let inherited = b.op(O::I32And, &[inherited, has_body], I32);
    let check_body = b.body.add_block();
    let metadata = b.body.add_block();
    b.branch(inherited, check_body, metadata);
    b.block = check_body;
    let used = b.load(input, BODY_USED, I32);
    reject(&mut b, r, frame, used);
    b.jump(metadata, &[]);
    b.block = metadata;
    let request = b.allocate(r.allocator.realloc, 48, 4);
    b.store(frame, 32, request, I32);
    let size = b.integer(48);
    b.effect(O::MemoryFill { mem: memory }, &[request, zero, size]);
    let four = b.integer(4);
    let backlink = b.op(O::I32Sub, &[request, four], I32);
    let header = b.load(backlink, 0, I32);
    let kind = b.integer(15);
    b.store(header, 16, kind, I32);
    b.store(request, METHOD, method, I32);
    b.store(request, REDIRECT, redirect, I32);
    let data = b.load(url, 0, I32);
    let length = b.load(url, 4, I32);
    let maximum = b.integer((u32::MAX - 64) / 4);
    let fits = b.op(O::I32LeU, &[length, maximum], I32);
    b.require(fits);
    let capacity = b.op(O::I32Mul, &[length, four], I32);
    let extra = b.integer(64);
    let capacity = b.op(O::I32Add, &[capacity, extra], I32);
    let parts = b.call(r.allocator.realloc, &[zero, zero, one, capacity], &[I32])[0];
    b.store(request, URL_PARTS, parts, I32);
    let invalid = b.call(
        r.imports["fetch_url"],
        &[data, length, parts, capacity],
        &[I32],
    )[0];
    reject(&mut b, r, frame, invalid);
    let prefix = b.integer(32);
    let normalized = b.op(O::I32Add, &[parts, prefix], I32);
    let length = b.load(parts, 20, I32);
    let url = b.call(r.strings.lift_canonical, &[normalized, length], &[I32])[0];
    b.store(request, URL, url, I32);
    let data = b.load(method, 0, I32);
    let length = b.load(method, 4, I32);
    let tag = b.call(r.imports["fetch_method"], &[data, length], &[I32])[0];
    let invalid = b.integer(u32::MAX);
    let invalid = b.op(O::I32Eq, &[tag, invalid], I32);
    reject(&mut b, r, frame, invalid);
    let get_head = b.op(O::I32LeU, &[tag, one], I32);
    let prohibited = b.op(O::I32And, &[get_head, has_body], I32);
    reject(&mut b, r, frame, prohibited);
    b.store(request, METHOD_TAG, tag, I32);
    let mut canonical = method;
    for (number, name) in [
        (0, "GET"),
        (1, "HEAD"),
        (2, "POST"),
        (3, "PUT"),
        (4, "DELETE"),
        (6, "OPTIONS"),
        (8, "PATCH"),
    ] {
        let number = b.integer(number);
        let matches = b.op(O::I32Eq, &[tag, number], I32);
        let name = b.integer(r.pool.get(name).unwrap());
        canonical = b.op(O::Select, &[name, canonical, matches], I32);
    }
    b.store(request, METHOD, canonical, I32);
    let mut valid = zero;
    let mut mode = zero;
    for (index, name) in ["follow", "manual", "error"].into_iter().enumerate() {
        let name = b.integer(r.pool.get(name).unwrap());
        let diff = b.call(r.strings.str_compare, &[redirect, name], &[I32])[0];
        let matches = b.op(O::I32Eqz, &[diff], I32);
        valid = b.op(O::I32Or, &[valid, matches], I32);
        let index = b.integer(index as u32);
        mode = b.op(O::Select, &[index, mode, matches], I32);
    }
    let invalid = b.op(O::I32Eqz, &[valid], I32);
    reject(&mut b, r, frame, invalid);
    b.store(request, REDIRECT_MODE, mode, I32);
    let fields = b.call(r.headers.new, &[header_mode, headers], &[I32, F64]);
    reject(&mut b, r, frame, fields[0]);
    let fields = b.op(O::I32TruncF64U, &[fields[1]], I32);
    b.store(frame, 36, fields, I32);
    let name = b.integer(r.pool.get("content-type").unwrap());
    let value = b.integer(r.pool.get("text/plain;charset=UTF-8").unwrap());
    let result = b.call(
        r.headers.default_type,
        &[fields, body_kind, name, value],
        &[I32, F64],
    );
    reject(&mut b, r, frame, result[0]);
    for at in [4, 8] {
        let value = b.load(fields, at, I32);
        b.store(request, at, value, I32);
    }
    let supplied_body = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(has_body, supplied_body, ready);
    b.block = supplied_body;
    let share = b.body.add_block();
    let snapshot = b.body.add_block();
    let stored = b.body.add_block();
    let owned_body = b.body.add_blockparam(stored, I32);
    b.branch(inherited, share, snapshot);
    b.block = share;
    b.store(input, BODY_USED, one, I32);
    b.jump(stored, &[body]);
    b.block = snapshot;
    let copied = b.call(r.bytes.copy, &[body], &[I32])[0];
    b.jump(stored, &[copied]);
    b.block = stored;
    b.store(request, BODY, owned_body, I32);
    b.store(request, BODY_KIND, body_kind, I32);
    b.jump(ready, &[]);
    b.block = ready;
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let payload = b.op(O::F64ConvertI32U, &[request], F64);
    b.ret(&[zero, payload]);
    b.finish(module, function)?;
    Ok(Helpers { new: function })
}
