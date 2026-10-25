//! Construction and checked payload access for closed tagged unions.
use crate::operation_schema::op_schema;
use crate::signatures::{Accepts, OutputContract, Signature, Ty};
use crate::{
    optional::{absence, optional_value},
    types::{StringType, VariantType},
};
use pliron::{
    builtin::{attributes::StringAttr, op_interfaces::OneResultInterface},
    common_traits::Verify,
    context::Context,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};

pub fn variant_cases(ctx: &Context, ty: TypeHandle) -> Result<Vec<(String, Option<TypeHandle>)>> {
    let ty = ty.deref(ctx);
    if let Some(union) = ty.downcast_ref::<crate::ValueUnionType>() {
        return variant_cases(ctx, union.variants());
    }
    if let Some(variant) = ty.downcast_ref::<VariantType>() {
        return Ok(variant.cases().to_vec());
    }
    verify_err!(pliron::location::Location::Unknown, InvalidVariant)
}

pub fn value_tag(ctx: &Context, ty: TypeHandle) -> Option<&'static str> {
    let ty = ty.deref(ctx);
    if ty.is::<pliron::builtin::types::FP64Type>() {
        Some("number")
    } else if ty.is::<StringType>() {
        Some("string")
    } else if ty
        .downcast_ref::<pliron::builtin::types::IntegerType>()
        .is_some_and(|ty| ty.width() == 1)
    {
        Some("boolean")
    } else if ty
        .downcast_ref::<crate::RecordType>()
        .is_some_and(|ty| ty.kind() == crate::RecordKind::Object)
    {
        Some("object")
    } else {
        None
    }
}

#[derive(Debug)]
struct InvalidVariant;

impl core::fmt::Display for InvalidVariant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("variant operation has an invalid tag or payload type")
    }
}

impl core::error::Error for InvalidVariant {}

op_schema! {
    pub VariantNewOp(..operands) -> infallible;
    name = "perry.variant_new";
    format = "attr($perry_variant_tag, $StringAttr) ` (` operands(CharSpace(`,`)) `) : ` type($0)";
    attributes = (perry_variant_tag: StringAttr);
    constructors {
        pub new(ty: TypeHandle, tag: &str, payload: Option<Value>)
            operands(payload.into_iter().collect()) => explicit(ty);
    }
    attribute(tag: String, get_attr_perry_variant_tag, set_attr_perry_variant_tag) =
        StringAttr::from(tag) => InvalidVariant;
    prepare(this) {
        <Self as OneResultInterface>::verify(this, ctx)?;
        let ty = this.get_result(ctx).get_type(ctx);
        let cases = variant_cases(ctx, ty)?;
    }
    result(ctx, ty: TypeHandle): TypeHandle = ty;
    error = InvalidVariant;
    check cases.iter().any(|(name, payload)| {
        name == tag.as_str()
            && (Signature { inputs: payload.iter().copied(), outputs: [ty] })
                .accepts(ctx, operands)
    }) => InvalidVariant;
}

op_schema! {
    pub VariantTagOp(value) -> infallible;
    name = "perry.variant_tag";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(); }
    result(ctx): Ty = Ty::String;
    error = InvalidVariant;
    check_before variant_cases(ctx, value.get_type(ctx)).is_ok() => InvalidVariant;
}

pub fn admits_payload(ctx: &Context, result: TypeHandle, payload: Option<TypeHandle>) -> bool {
    match payload {
        Some(payload) => result == payload || optional_value(ctx, result) == Some(payload),
        None => absence(ctx, result).is_some_and(|a| a.admits_undefined()),
    }
}

op_schema! {
    pub VariantPayloadOp(value) -> infallible;
    name = "perry.variant_payload";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value, result: TypeHandle) => explicit(result); }
    prepare(_this) { let cases = variant_cases(ctx, value.get_type(ctx))?; }
    result(ctx, cases: Vec<(String, Option<TypeHandle>)>): impl OutputContract =
        Accepts(move |ctx: &Context, result| {
            cases
                .iter()
                .any(|(_, payload)| admits_payload(ctx, result, *payload))
        });
    error = InvalidVariant;
}
