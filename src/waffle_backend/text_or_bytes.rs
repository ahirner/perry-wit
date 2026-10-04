//! The string/byte-view union uses descriptor alignment to retain its runtime kind.
//!
//! Strings have tag zero; byte views have tag one. Both descriptors are aligned to
//! four bytes. The collector accepts interior pointers, so tagged views keep their
//! descriptors and backing allocations alive without a separate wrapper object.

use anyhow::Result;
use perry_hir::types::Type as HirType;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

pub(crate) fn is_text_or_bytes(ty: &HirType) -> bool {
    matches!(ty, HirType::Union(types) if types.len() == 2
        && types.contains(&HirType::String)
        && types.iter().any(super::bytes::is_byte_view))
}

pub(crate) fn value_type() -> HirType {
    HirType::Union(vec![HirType::String, HirType::Named("Uint8Array".into())])
}

pub(crate) fn emit_lift(
    module: &mut Module<'static>,
    memory: Memory,
    text: Func,
    bytes: Func,
) -> Result<Func> {
    let functions = super::runtime::emit_functions(
        module,
        memory,
        r#"
        (module
          (import "host" "text" (func $text (param i32 i32) (result i32)))
          (import "host" "bytes" (func $bytes (param i32 i32) (result i32)))
          (memory 1)
          (func (export "value.lift") (param $tag i32) (param $data i32) (param $length i32) (result i32)
            (if (i32.gt_u (local.get $tag) (i32.const 1)) (then unreachable))
            (if (result i32) (local.get $tag)
              (then (i32.or (call $bytes (local.get $data) (local.get $length)) (i32.const 1)))
              (else (call $text (local.get $data) (local.get $length))))))
    "#,
        &BTreeMap::from([("text", text), ("bytes", bytes)]),
    )?;
    Ok(functions["value.lift"])
}

pub(crate) fn contains(ty: &HirType) -> bool {
    if is_text_or_bytes(ty) {
        return true;
    }
    match ty {
        HirType::Promise(inner) | HirType::Array(inner) => contains(inner),
        HirType::Generic { type_args, .. } | HirType::Union(type_args) => {
            type_args.iter().any(contains)
        }
        _ => false,
    }
}

pub(crate) fn equivalent(left: &HirType, right: &HirType) -> bool {
    left == right
        || (super::objects::is_object(left) && super::objects::is_object(right))
        || (is_text_or_bytes(left) && is_text_or_bytes(right))
        || matches!((left, right), (HirType::Promise(left), HirType::Promise(right)) if equivalent(left, right))
}
