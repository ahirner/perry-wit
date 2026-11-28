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
};

impl VariantType {
    /// The case layout shared by tagged variants and JavaScript value unions.
    pub fn of(ctx: &Context, ty: TypeHandle) -> Result<std::cell::Ref<'_, Self>> {
        let variants = ty
            .deref(ctx)
            .downcast_ref::<crate::ValueUnionType>()
            .map_or(ty, |union| union.variants());
        std::cell::Ref::filter_map(variants.deref(ctx), |ty| ty.downcast_ref::<Self>())
            .map_err(|_| pliron::verify_error_noloc!(InvalidVariant))
    }

    /// A known tag can have an empty payload.
    pub fn case(&self, tag: &str) -> Option<Option<TypeHandle>> {
        let index = self
            .cases()
            .binary_search_by(|(name, _)| name.as_str().cmp(tag))
            .ok()?;
        self.cases().get(index).map(|(_, payload)| *payload)
    }
}

pub fn variant_cases(ctx: &Context, ty: TypeHandle) -> Result<Vec<(String, Option<TypeHandle>)>> {
    Ok(VariantType::of(ctx, ty)?.cases().to_vec())
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
    } else if ty.is::<crate::BytesType>()
        || ty
            .downcast_ref::<crate::RecordType>()
            .is_some_and(|ty| ty.kind() == crate::RecordKind::Object)
    {
        Some("object")
    } else {
        None
    }
}

#[derive(Debug, thiserror::Error)]
#[error("variant operation has an invalid tag or payload type")]
struct InvalidVariant;

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
        let variant = VariantType::of(ctx, ty)?;
    }
    result(ctx, ty: TypeHandle): TypeHandle = ty;
    error = InvalidVariant;
    check variant.case(tag.as_str()).is_some_and(|payload|
        (Signature { inputs: payload, outputs: [ty] }).accepts(ctx, operands)
    ) => InvalidVariant;
}

op_schema! {
    pub VariantTagOp(value) -> infallible;
    name = "perry.variant_tag";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value) => infer(); }
    result(ctx): Ty = Ty::String;
    error = InvalidVariant;
    check_before VariantType::of(ctx, value.get_type(ctx)).is_ok() => InvalidVariant;
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
