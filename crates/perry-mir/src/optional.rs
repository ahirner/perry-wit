//! `T | undefined` values (`perry.optional`).
//!
//! The builder lets optional values flow through locals unchanged and applies ECMAScript
//! coercions at their uses: arithmetic and ordering see `undefined` as `NaN`, string
//! concatenation as `"undefined"`, strict equality compares definedness first. A boundary that
//! is typed `T` (parameter, return, field, element) unwraps and throws a catchable `TypeError`
//! on `undefined`, as does property access on `undefined`.
use crate::{
    NewOp,
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

/// Fails verification of `op`, named `name`, unless its result type is `ok`.
fn check(ctx: &Context, op: &dyn Op, ok: bool, name: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        verify_err!(op.loc(ctx), OptionalErr::Result(name))
    }
}

/// The absence states of `op`'s optional result type.
fn result_absence(ctx: &Context, op: &dyn Op) -> Option<Absence> {
    absence(ctx, op.get_operation().deref(ctx).get_type(0))
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
            op: Self::new_op(ctx, vec![ty], vec![]),
        }
    }
}

impl Verify for UndefinedOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let admits = result_absence(ctx, self).is_some_and(Absence::admits_undefined);
        check(ctx, self, admits, "perry.undefined")
    }
}

/// The omitted-property sentinel of an optional record field.
#[pliron_op(
    name = "perry.omitted",
    format = "` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface]
)]
pub struct OmittedOp;

impl OmittedOp {
    pub fn with_type(ctx: &mut Context, ty: TypeHandle) -> Self {
        OmittedOp {
            op: Self::new_op(ctx, vec![ty], vec![]),
        }
    }
}

impl Verify for OmittedOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let admits = result_absence(ctx, self).is_some_and(Absence::admits_undefined);
        check(ctx, self, admits, "perry.omitted")
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
            op: Self::new_op(ctx, vec![ty], vec![value]),
        }
    }
}

impl Verify for WrapOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let value = operand(self, ctx).get_type(ctx);
        let wraps = optional_value(ctx, self.get_result(ctx).get_type(ctx)) == Some(value);
        check(ctx, self, wraps, "perry.wrap")
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
            op: Self::new_op(ctx, vec![ty], vec![value]),
        }
    }
}

impl Verify for IsUndefinedOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        inner(ctx, operand(self, ctx), "perry.is_undefined")?;
        check(
            ctx,
            self,
            is_bool(ctx, self.get_result(ctx)),
            "perry.is_undefined",
        )
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
            op: Self::new_op(ctx, vec![ty], vec![value]),
        }
    }
}

impl Verify for ToNumberOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let number: TypeHandle = FP64Type::get(ctx).into();
        let numeric: TypeHandle = crate::types::NumericType::get(ctx).into();
        let inner_ty = inner(ctx, operand(self, ctx), "perry.to_number")?;
        let res_ty = self.get_result(ctx).get_type(ctx);
        let ok = (inner_ty == number || inner_ty == numeric) && res_ty == inner_ty;
        check(ctx, self, ok, "perry.to_number")
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
            op: Self::new_op(ctx, vec![ty], vec![value]),
        }
    }
}

impl Verify for ToStringOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let string: TypeHandle = StringType::get(ctx).into();
        let ok = inner(ctx, operand(self, ctx), "perry.to_string")? == string
            && self.get_result(ctx).get_type(ctx) == string;
        check(ctx, self, ok, "perry.to_string")
    }
}

side_effects!(false; UndefinedOp, OmittedOp, WrapOp, IsUndefinedOp, ToNumberOp, ToStringOp, NullOp, IsNullOp);

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
            op: Self::new_op(ctx, vec![ty], vec![]),
        }
    }
}

impl Verify for NullOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let admits = result_absence(ctx, self).is_some_and(Absence::admits_null);
        check(ctx, self, admits, "perry.null")
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
            op: Self::new_op(ctx, vec![ty], vec![value]),
        }
    }
}

impl Verify for IsNullOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        inner(ctx, operand(self, ctx), "perry.is_null")?;
        check(
            ctx,
            self,
            is_bool(ctx, self.get_result(ctx)),
            "perry.is_null",
        )
    }
}

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
            op: Self::new_op(ctx, vec![target], vec![value]),
        }
    }
}

impl Verify for OptionalCastOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let source = inner(ctx, operand(self, ctx), "perry.optional_cast")?;
        let casts = optional_value(ctx, self.get_result(ctx).get_type(ctx)) == Some(source);
        check(ctx, self, casts, "perry.optional_cast")
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
            op: Self::new_op(ctx, vec![ty], vec![value]),
        })
    }
}

impl Verify for UnwrapOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let value = inner(ctx, operand(self, ctx), "perry.unwrap")?;
        check(
            ctx,
            self,
            value == self.get_result(ctx).get_type(ctx),
            "perry.unwrap",
        )
    }
}

#[op_interface_impl]
impl MayThrowInterface for UnwrapOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
