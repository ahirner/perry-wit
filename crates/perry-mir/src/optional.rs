//! `T | undefined` values (`perry.optional`).
//!
//! The builder lets optional values flow through locals unchanged and applies ECMAScript
//! coercions at their uses: arithmetic and ordering see `undefined` as `NaN`, string
//! concatenation as `"undefined"`, strict equality compares definedness first. A boundary that
//! is typed `T` (parameter, return, field, element) unwraps and throws a catchable `TypeError`
//! on `undefined`, as does property access on `undefined`.
use crate::{
    attrs::{Absence, ErrorKind},
    exceptions::MayThrowInterface,
    scalar::is_bool,
    types::{OptionalType, StringType},
};
use pliron::{
    builtin::{
        op_interfaces::{NOpdsInterface, OneOpdInterface, OneResultInterface},
        types::{FP64Type, IntegerType, Signedness},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum OptionalErr {
    #[error("{0} expects a perry.optional operand")]
    NotOptional(&'static str),
    #[error("{0} result type does not match its operand")]
    Result(&'static str),
}

/// The `T` of an optional type, if `ty` is one.
pub fn optional_value(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    optional_parts(ctx, ty).map(|(value, _)| value)
}

/// The absence states (`null`, `undefined` or both) of an optional type, if `ty` is one.
pub fn absence(ctx: &Context, ty: TypeHandle) -> Option<Absence> {
    optional_parts(ctx, ty).map(|(_, states)| states)
}

/// The value type and absence states of an optional type, in one lookup.
pub fn optional_parts(ctx: &Context, ty: TypeHandle) -> Option<(TypeHandle, Absence)> {
    TypedHandle::<OptionalType>::from_handle(ty, ctx)
        .ok()
        .map(|optional| {
            let optional = optional.deref(ctx);
            (optional.value(), optional.absence())
        })
}

fn operand(op: &dyn Op, ctx: &Context) -> Value {
    op.get_operation().deref(ctx).get_operand(0)
}

fn inner(ctx: &Context, value: Value, op: &'static str) -> Result<TypeHandle> {
    match optional_value(ctx, value.get_type(ctx)) {
        Some(ty) => Ok(ty),
        None => verify_err!(
            pliron::location::Location::Unknown,
            OptionalErr::NotOptional(op)
        ),
    }
}

macro_rules! pure {
    ($($op:ty),*) => {$(
        #[op_interface_impl]
        impl SideEffects for $op {
            fn has_side_effects(&self, _ctx: &Context) -> bool {
                false
            }
        }
    )*};
}

/// The `undefined` value of an optional type.
#[pliron_op(
    name = "perry.undefined",
    format = "` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface]
)]
pub struct UndefinedOp;

impl UndefinedOp {
    pub fn new(ctx: &mut Context, value: TypeHandle) -> Self {
        let ty = OptionalType::get(ctx, value).into();
        Self::with_type(ctx, ty)
    }

    pub fn with_type(ctx: &mut Context, ty: TypeHandle) -> Self {
        UndefinedOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for UndefinedOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !absence(ctx, self.get_result(ctx).get_type(ctx)).is_some_and(Absence::admits_undefined)
        {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.undefined"))?
        }
        Ok(())
    }
}

/// A defined value viewed as `T | undefined`.
#[pliron_op(
    name = "perry.wrap",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct WrapOp;

impl WrapOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = OptionalType::get(ctx, value.get_type(ctx)).into();
        Self::with_type(ctx, value, ty)
    }

    pub fn with_type(ctx: &mut Context, value: Value, ty: TypeHandle) -> Self {
        WrapOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for WrapOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let value = operand(self, ctx).get_type(ctx);
        if optional_value(ctx, self.get_result(ctx).get_type(ctx)) != Some(value) {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.wrap"))?
        }
        Ok(())
    }
}

/// Whether an optional value is `undefined`.
#[pliron_op(
    name = "perry.is_undefined",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct IsUndefinedOp;

impl IsUndefinedOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        IsUndefinedOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for IsUndefinedOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        inner(ctx, operand(self, ctx), "perry.is_undefined")?;
        if !is_bool(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.is_undefined"))?
        }
        Ok(())
    }
}

/// ECMAScript `ToNumber` on `number | undefined`: `undefined` becomes `NaN`.
/// With the NaN-payload representation this is a canonicalization, not a branch.
#[pliron_op(
    name = "perry.to_number",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct ToNumberOp;

impl ToNumberOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = FP64Type::get(ctx).into();
        ToNumberOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ToNumberOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let number = FP64Type::get(ctx).into();
        if inner(ctx, operand(self, ctx), "perry.to_number")? != number
            || self.get_result(ctx).get_type(ctx) != number
        {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.to_number"))?
        }
        Ok(())
    }
}

/// ECMAScript `ToString` on `string | undefined`: `undefined` becomes `"undefined"`.
#[pliron_op(
    name = "perry.to_string",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct ToStringOp;

impl ToStringOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = StringType::get(ctx).into();
        ToStringOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ToStringOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let string: TypeHandle = StringType::get(ctx).into();
        if inner(ctx, operand(self, ctx), "perry.to_string")? != string
            || self.get_result(ctx).get_type(ctx) != string
        {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.to_string"))?
        }
        Ok(())
    }
}

pure!(UndefinedOp, WrapOp, IsUndefinedOp, ToNumberOp, ToStringOp);

/// The explicit `null` value of a nullable type.
#[pliron_op(
    name = "perry.null",
    format = "` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface]
)]
pub struct NullOp;

impl NullOp {
    pub fn new(ctx: &mut Context, ty: TypeHandle) -> Self {
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for NullOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !absence(ctx, self.get_result(ctx).get_type(ctx)).is_some_and(Absence::admits_null) {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.null"))?
        }
        Ok(())
    }
}

/// Whether a nullable value is `null`.
#[pliron_op(
    name = "perry.is_null",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct IsNullOp;

impl IsNullOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for IsNullOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        inner(ctx, operand(self, ctx), "perry.is_null")?;
        if !is_bool(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.is_null"))?
        }
        Ok(())
    }
}

pure!(NullOp, IsNullOp);

/// Change admitted absence states, checking any states excluded by the target type.
#[pliron_op(
    name = "perry.optional_cast",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct OptionalCastOp;

impl OptionalCastOp {
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

impl Verify for OptionalCastOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let source = inner(ctx, operand(self, ctx), "perry.optional_cast")?;
        if optional_value(ctx, self.get_result(ctx).get_type(ctx)) != Some(source) {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.optional_cast"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl MayThrowInterface for OptionalCastOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}

/// The defined value; throws a catchable `TypeError` on `undefined`. Emitted at typed
/// boundaries and before property access, where ECMAScript also throws `TypeError`.
#[pliron_op(
    name = "perry.unwrap",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct UnwrapOp;

impl UnwrapOp {
    pub fn new(ctx: &mut Context, value: Value) -> Result<Self> {
        let ty = inner(ctx, value, "perry.unwrap")?;
        Ok(UnwrapOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![value],
                vec![],
                0,
            ),
        })
    }
}

impl Verify for UnwrapOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if inner(ctx, operand(self, ctx), "perry.unwrap")? != self.get_result(ctx).get_type(ctx) {
            verify_err!(self.loc(ctx), OptionalErr::Result("perry.unwrap"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl MayThrowInterface for UnwrapOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
