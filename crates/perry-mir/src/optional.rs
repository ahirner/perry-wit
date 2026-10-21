//! `T | undefined` values (`perry.optional`).
//!
//! The builder lets optional values flow through locals unchanged and applies ECMAScript
//! coercions at their uses: arithmetic and ordering see `undefined` as `NaN`, string
//! concatenation as `"undefined"`, strict equality compares definedness first. A boundary that
//! is typed `T` (parameter, return, field, element) unwraps and throws a catchable `TypeError`
//! on `undefined`, as does property access on `undefined`.
use crate::operation_schema::op_schema;
use crate::signatures::{Accepts, OptionalPayload, OutputContract, ResultType, Ty};
use crate::{attrs::Absence, types::OptionalType};
use pliron::{
    common_traits::Verify,
    context::Context,
    op::Op,
    operation::Operation,
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

/// The payload of an optional type, or `ty` when it has no absence states.
pub fn present_type(ctx: &Context, ty: TypeHandle) -> TypeHandle {
    optional_value(ctx, ty).unwrap_or(ty)
}

/// The absence states (`null`, `undefined` or both) of an optional type, if `ty` is one.
pub fn absence(ctx: &Context, ty: TypeHandle) -> Option<Absence> {
    optional_parts(ctx, ty).map(|(_, states)| states)
}

/// Whether `ty` accepts an absent property or an explicit `undefined` value.
pub fn admits_undefined(ctx: &Context, ty: TypeHandle) -> bool {
    absence(ctx, ty).is_some_and(|states| states.admits_undefined())
}

/// Whether a field write can change property presence or insertion order.
pub fn field_order_may_change(ctx: &Context, ty: TypeHandle) -> bool {
    admits_undefined(ctx, ty) || present_type(ctx, ty).deref(ctx).is::<crate::JsonType>()
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

fn inner(ctx: &Context, value: Value, op: &'static str) -> Result<TypeHandle> {
    match optional_value(ctx, value.get_type(ctx)) {
        Some(ty) => Ok(ty),
        None => verify_err!(
            pliron::location::Location::Unknown,
            OptionalErr::NotOptional(op)
        ),
    }
}

op_schema! {
    /// The `undefined` value of an optional type.
    pub UndefinedOp() -> infallible;
    name = "perry.undefined";
    format = "` : ` type($0)";
    constructors { pub new(value: TypeHandle) => explicit(OptionalType::get(ctx, value).into());
        pub with_type(ty: TypeHandle) => explicit(ty); }
    result(ctx): impl OutputContract = Accepts(|ctx: &Context, ty| {
        absence(ctx, ty).is_some_and(Absence::admits_undefined)
    });
    error = OptionalErr::Result("perry.undefined");
}

op_schema! {
    /// The omitted-property sentinel of an optional record field.
    pub OmittedOp() -> infallible;
    name = "perry.omitted";
    format = "` : ` type($0)";
    constructors { pub with_type(ty: TypeHandle) => explicit(ty); }
    result(ctx): impl OutputContract = Accepts(|ctx: &Context, ty| {
        absence(ctx, ty).is_some_and(Absence::admits_undefined)
    });
    error = OptionalErr::Result("perry.omitted");
}

op_schema! {
    /// A defined value viewed as `T | undefined`.
    pub WrapOp(value) -> infallible;
    name = "perry.wrap";
    format = "$0 ` : ` type($0)";
    constructors {
        pub new(value: Value) => infer(value);
        pub with_type(value: Value, ty: TypeHandle) => explicit(ty);
    }
    result(ctx, value: Value): OptionalPayload =
        OptionalPayload(value.get_type(ctx));
    error = OptionalErr::Result("perry.wrap");
}

op_schema! {
    /// Whether an optional value is `undefined`.
    pub IsUndefinedOp(value) -> infallible;
    name = "perry.is_undefined";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(); }
    result(ctx): Ty = Ty::Predicate;
    error = OptionalErr::Result("perry.is_undefined");
    require inner(ctx, value, "perry.is_undefined")?;
}

op_schema! {
    /// ECMAScript `ToNumber` on `number | undefined`: `undefined` becomes `NaN`.
    /// With the NaN-payload representation this is a canonicalization, not a branch.
    pub ToNumberOp(value) -> infallible;
    name = "perry.to_number";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => explicit(Ty::Number.get(ctx)); }
    prepare(_this) { let source = inner(ctx, value, "perry.to_number")?; }
    result(ctx, source: TypeHandle): impl OutputContract = Accepts(move |ctx: &Context, ty| {
        (source == Ty::Number.get(ctx) || source == crate::types::NumericType::get(ctx).into())
            && ty == source
    });
    error = OptionalErr::Result("perry.to_number");
}

op_schema! {
    /// ECMAScript `ToString` on `string | undefined`: `undefined` becomes `"undefined"`.
    pub ToStringOp(value) -> infallible;
    name = "perry.to_string";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(); }
    result(ctx): Ty = Ty::String;
    error = OptionalErr::Result("perry.to_string");
    check_before inner(ctx, value, "perry.to_string")? == Ty::String.get(ctx)
        => OptionalErr::Result("perry.to_string");
}

op_schema! {
    /// The explicit `null` value of a nullable type.
    pub NullOp() -> infallible;
    name = "perry.null";
    format = "` : ` type($0)";
    constructors { pub new(ty: TypeHandle) => explicit(ty); }
    result(ctx): impl OutputContract = Accepts(|ctx: &Context, ty| {
        absence(ctx, ty).is_some_and(Absence::admits_null)
    });
    error = OptionalErr::Result("perry.null");
}

op_schema! {
    /// Whether a nullable value is `null`.
    pub IsNullOp(value) -> infallible;
    name = "perry.is_null";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(); }
    result(ctx): Ty = Ty::Predicate;
    error = OptionalErr::Result("perry.is_null");
    require inner(ctx, value, "perry.is_null")?;
}

op_schema! {
    /// Change admitted absence states, checking any states excluded by the target type.
    pub OptionalCastOp(value) -> infallible;
    name = "perry.optional_cast";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value, target: TypeHandle) => explicit(target); }
    prepare(_this) { let source = inner(ctx, value, "perry.optional_cast")?; }
    result(ctx, source: TypeHandle): OptionalPayload =
        OptionalPayload(source);
    error = OptionalErr::Result("perry.optional_cast");
}

op_schema! {
    /// Extract the defined value or throw a catchable `TypeError` on `undefined`. Emitted at
    /// typed boundaries and before property access, matching ECMAScript `TypeError` semantics.
    pub UnwrapOp(value) -> fallible;
    name = "perry.unwrap";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(value); }
    result(ctx, value: Value): TypeHandle = inner(ctx, value, "perry.unwrap");
    error = OptionalErr::Result("perry.unwrap");
}
