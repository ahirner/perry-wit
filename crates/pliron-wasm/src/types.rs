//! Mapping between pliron builtin types and Wasm value types.
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        types::{FP32Type, FP64Type, IntegerType, Signedness},
    },
    context::Context,
    r#type::TypeHandle,
    utils::apint::APInt,
};
use std::num::NonZeroUsize;
use wasm_encoder::ValType;

/// The Wasm value type of a pliron type: signless `i32`/`i64`, `fp32`, `fp64`.
pub(crate) fn val_type(ctx: &Context, ty: TypeHandle) -> Option<ValType> {
    let ty = ty.deref(ctx);
    if let Some(int) = ty.downcast_ref::<IntegerType>() {
        if int.signedness() != Signedness::Signless {
            return None;
        }
        return match int.width() {
            32 => Some(ValType::I32),
            64 => Some(ValType::I64),
            _ => None,
        };
    }
    if ty.is::<FP64Type>() {
        Some(ValType::F64)
    } else if ty.is::<FP32Type>() {
        Some(ValType::F32)
    } else {
        None
    }
}

/// The pliron type for a numeric Wasm value type.
pub(crate) fn pliron_type(ctx: &mut Context, ty: ValType) -> TypeHandle {
    match ty {
        ValType::I32 => IntegerType::get(ctx, 32, Signedness::Signless).into(),
        ValType::I64 => IntegerType::get(ctx, 64, Signedness::Signless).into(),
        ValType::F32 => FP32Type::get(ctx).into(),
        ValType::F64 => FP64Type::get(ctx).into(),
        other => unreachable!(
            "NumOpcode and MemKind, the only sources of value types, name no {other:?} values"
        ),
    }
}

/// Bit width of `i32`.
pub(crate) const I32_BITS: NonZeroUsize = NonZeroUsize::new(32).expect("32 is non-zero");
/// Bit width of `i64`.
pub(crate) const I64_BITS: NonZeroUsize = NonZeroUsize::new(64).expect("64 is non-zero");

/// A signless integer attribute of `bits` width holding `value`.
pub(crate) fn integer_attr(ctx: &mut Context, bits: NonZeroUsize, value: u64) -> IntegerAttr {
    let ty = IntegerType::get(ctx, bits.get() as u32, Signedness::Signless);
    IntegerAttr::new(ty, APInt::from_u64(value, bits))
}
