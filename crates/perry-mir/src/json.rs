//! Views of statically typed storage as JSON, without copying aggregates.
use crate::{
    optional::optional_value,
    types::{ArrayType, JsonType, OptionalType, RecordType, StringType},
};
use pliron::{
    builtin::{
        op_interfaces::{OneOpdInterface, OneResultInterface},
        types::{FP64Type, IntegerType},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};
use std::collections::HashSet;

/// Whether the full reachable type graph has JSON-compatible values.
pub fn compatible(ctx: &Context, ty: TypeHandle) -> bool {
    fn visit(ctx: &Context, ty: TypeHandle, seen: &mut HashSet<TypeHandle>) -> bool {
        if !seen.insert(ty) {
            return true;
        }
        if let Some(inner) = optional_value(ctx, ty) {
            return visit(ctx, inner, seen);
        }
        let ty = ty.deref(ctx);
        if ty
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() == 1)
        {
            return true;
        }
        if ty.is::<FP64Type>() || ty.is::<StringType>() || ty.is::<JsonType>() {
            return true;
        }
        if let Some(array) = ty.downcast_ref::<ArrayType>() {
            return visit(ctx, array.element(), seen);
        }
        if let Some(record) = ty.downcast_ref::<RecordType>() {
            return record
                .fields()
                .is_ok_and(|fields| fields.iter().all(|(_, ty)| visit(ctx, *ty, seen)));
        }
        false
    }
    visit(ctx, ty, &mut HashSet::new())
}

#[derive(Debug, thiserror::Error)]
#[error("perry.json_pack requires JSON-compatible storage and a matching JSON result")]
struct PackError;

/// Scalar conversion or an alias-preserving view over a native record or array.
#[pliron_op(name = "perry.json_pack", format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
pub struct PackOp;
impl PackOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let json = JsonType::get(ctx).into();
        let result = if optional_value(ctx, value.get_type(ctx)).is_some() {
            OptionalType::get(ctx, json).into()
        } else {
            json
        };
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for PackOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let operand = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        let result = self.get_result(ctx).get_type(ctx);
        let inner = optional_value(ctx, result).unwrap_or(result);
        if !compatible(ctx, operand)
            || !inner.deref(ctx).is::<JsonType>()
            || optional_value(ctx, operand).is_some() != optional_value(ctx, result).is_some()
        {
            verify_err!(self.loc(ctx), PackError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for PackOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Validate a JSON value and retain an alias of aggregate storage at the requested type.
#[pliron_op(name = "perry.json_unpack", format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
pub struct UnpackOp;
impl UnpackOp {
    pub fn new(ctx: &mut Context, value: Value, target: TypeHandle) -> Self {
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![target],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for UnpackOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let operand = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        let inner = optional_value(ctx, operand).unwrap_or(operand);
        if !inner.deref(ctx).is::<JsonType>()
            || !compatible(ctx, self.get_result(ctx).get_type(ctx))
        {
            verify_err!(self.loc(ctx), UnpackError)?;
        }
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
#[error("perry.json_unpack requires JSON input and a JSON-compatible target")]
struct UnpackError;
#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for UnpackOp {
    fn exception(&self, _ctx: &Context) -> crate::attrs::ErrorKind {
        crate::attrs::ErrorKind::TypeError
    }
}
