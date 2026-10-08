//! Mapping between pliron builtin types and Wasm value types.
use pliron::{
    builtin::types::{FP32Type, FP64Type, IntegerType, Signedness},
    context::Context,
    r#type::TypeHandle,
};
use wasm_encoder::ValType;

/// The Wasm value type of a pliron type: signless `i32`/`i64`, `fp32`, `fp64`.
pub fn val_type(ctx: &Context, ty: TypeHandle) -> Option<ValType> {
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

/// The pliron type for a Wasm value type.
pub fn pliron_type(ctx: &mut Context, ty: ValType) -> TypeHandle {
    match ty {
        ValType::I32 => IntegerType::get(ctx, 32, Signedness::Signless).into(),
        ValType::I64 => IntegerType::get(ctx, 64, Signedness::Signless).into(),
        ValType::F32 => FP32Type::get(ctx).into(),
        ValType::F64 => FP64Type::get(ctx).into(),
        other => panic!("pliron-wasm does not model {other:?} values yet"),
    }
}
