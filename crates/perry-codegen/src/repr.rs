//! How MIR types are represented in Wasm values and memory slots.
pub(crate) use crate::values::Carriers;
use anyhow::{Result, bail};
use perry_mir::{ByteInputType, OptionalType, PromiseType, SlotType, TransferType, ValueKind};
use pliron::{
    builtin::{
        attributes::FPDoubleAttr,
        ops::ConstantOp,
        types::{FP64Type, IntegerType, Signedness},
    },
    context::Context,
    operation::Operation,
    printable::Printable,
    r#type::TypeHandle,
    value::Value,
};
use pliron_wasm::attrs::MemKind;

pub(crate) fn val_type_is_i64(ctx: &Context, ty: TypeHandle) -> bool {
    ty.deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|int| int.width() == 64)
}

pub(crate) fn i32_type(ctx: &Context) -> TypeHandle {
    IntegerType::get(ctx, 32, Signedness::Signless).into()
}

/// How a MIR type is represented in Wasm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Repr {
    F64,
    Bool,
    I32,
    I64,
    Ref,
}

/// The value of `value` when it is a constant non-zero `i32` integer, as a divisor.
pub(crate) fn nonzero_i32_constant(ctx: &Context, value: Value) -> Option<i32> {
    let op = value.defining_op()?;
    let constant = Operation::get_op::<ConstantOp>(op, ctx)?;
    let attr = constant.get_attr_builtin_constant_value(ctx)?;
    let double = f64::from(attr.downcast_ref::<FPDoubleAttr>()?.clone());
    let integer = double as i32;
    (integer != 0 && f64::from(integer) == double).then_some(integer)
}

pub(crate) fn repr(ctx: &Context, ty: TypeHandle) -> Result<Repr> {
    let deref = ty.deref(ctx);
    if deref.is::<FP64Type>() {
        return Ok(Repr::F64);
    }
    if deref.is::<perry_mir::NumericType>() {
        return Ok(Repr::I32);
    }
    if let Some(int) = deref.downcast_ref::<IntegerType>() {
        return match int.width() {
            1 => Ok(Repr::Bool),
            8 | 16 | 32 => Ok(Repr::I32),
            64 => Ok(Repr::I64),
            width => bail!("integers of width {width} are not supported"),
        };
    }
    if ValueKind::of(ctx, ty).is_some()
        || deref.is::<PromiseType>()
        || deref.is::<ByteInputType>()
        || deref.is::<TransferType>()
        || deref.is::<SlotType>()
    {
        return Ok(Repr::Ref);
    }
    if let Some(optional) = deref.downcast_ref::<OptionalType>() {
        let inner = optional.value();
        drop(deref);
        return repr(ctx, inner);
    }
    bail!("type {} has no Wasm representation", ty.disp(ctx))
}

pub(crate) fn wasm_type(ctx: &Context, ty: TypeHandle) -> Result<TypeHandle> {
    Ok(match repr(ctx, ty)? {
        Repr::F64 => FP64Type::get(ctx).into(),
        Repr::I64 => IntegerType::get(ctx, 64, Signedness::Signless).into(),
        _ => i32_type(ctx),
    })
}

pub(crate) fn slot_kind(ctx: &Context, ty: TypeHandle) -> Result<MemKind> {
    Ok(match repr(ctx, ty)? {
        Repr::F64 => MemKind::F64,
        Repr::I64 => MemKind::I64,
        _ => MemKind::I32,
    })
}

pub(crate) fn optional_inner(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    perry_mir::optional_value(ctx, ty)
}

/// Bytes an aggregate slot occupies: numeric alternatives keep a tag and a payload.
pub(crate) fn slot_size(ctx: &Context, ty: TypeHandle) -> u32 {
    if crate::values::is_numeric_type(ctx, ty) {
        perry_abi::layout::array::Elements::Numeric
    } else {
        perry_abi::layout::array::Elements::Scalar
    }
    .stride()
}
