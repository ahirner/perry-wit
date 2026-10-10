//! Construction and checked payload access for closed tagged unions.
use crate::{
    attrs::ErrorKind,
    exceptions::MayThrowInterface,
    optional::{absence, optional_value},
    types::{StringType, VariantType},
};
use pliron::{
    builtin::{
        attributes::StringAttr,
        op_interfaces::{OneOpdInterface, OneResultInterface},
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

pub fn cases(ctx: &Context, ty: TypeHandle) -> Result<Vec<(String, Option<TypeHandle>)>> {
    let ty = ty.deref(ctx);
    if let Some(variant) = ty.downcast_ref::<VariantType>() {
        return Ok(variant.cases().to_vec());
    }
    if ty.is::<crate::types::NumberStringType>() {
        return Ok(vec![
            (
                "number".into(),
                Some(pliron::builtin::types::FP64Type::get(ctx).into()),
            ),
            ("string".into(), Some(StringType::get(ctx).into())),
        ]);
    }
    verify_err!(pliron::location::Location::Unknown, InvalidVariant)
}

#[derive(Debug, thiserror::Error)]
#[error("variant operation has an invalid tag or payload type")]
struct InvalidVariant;

#[pliron_op(name = "perry.variant_new",
    format = "attr($perry_variant_tag, $StringAttr) ` (` operands(CharSpace(`,`)) `) : ` type($0)",
    interfaces = [OneResultInterface], attributes = (perry_variant_tag: StringAttr))]
pub struct VariantNewOp;
impl VariantNewOp {
    pub fn new(ctx: &mut Context, ty: TypeHandle, tag: &str, payload: Option<Value>) -> Self {
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                payload.into_iter().collect(),
                vec![],
                0,
            ),
        };
        op.set_attr_perry_variant_tag(ctx, StringAttr::from(tag));
        op
    }
    pub fn tag(&self, ctx: &Context) -> String {
        self.get_attr_perry_variant_tag(ctx)
            .expect("tag")
            .as_str()
            .to_owned()
    }
}
impl Verify for VariantNewOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let cases = cases(ctx, self.get_result(ctx).get_type(ctx))?;
        let operands: Vec<_> = self
            .get_operation()
            .deref(ctx)
            .operands()
            .map(|v| v.get_type(ctx))
            .collect();
        if !cases.iter().any(|(name, payload)| {
            *name == self.tag(ctx) && payload.iter().copied().eq(operands.iter().copied())
        }) {
            verify_err!(self.loc(ctx), InvalidVariant)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for VariantNewOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}

#[pliron_op(name = "perry.variant_tag", format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
pub struct VariantTagOp;
impl VariantTagOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = StringType::get(ctx).into();
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
impl Verify for VariantTagOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if cases(ctx, self.get_operand(ctx).get_type(ctx)).is_err()
            || !self
                .get_result(ctx)
                .get_type(ctx)
                .deref(ctx)
                .is::<StringType>()
        {
            verify_err!(self.loc(ctx), InvalidVariant)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for VariantTagOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}

pub fn admits_payload(ctx: &Context, result: TypeHandle, payload: Option<TypeHandle>) -> bool {
    match payload {
        Some(payload) => result == payload || optional_value(ctx, result) == Some(payload),
        None => absence(ctx, result).is_some_and(|a| a.admits_undefined()),
    }
}

#[pliron_op(name = "perry.variant_payload", format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
pub struct VariantPayloadOp;
impl VariantPayloadOp {
    pub fn new(ctx: &mut Context, value: Value, result: TypeHandle) -> Self {
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
impl Verify for VariantPayloadOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let cases = cases(ctx, self.get_operand(ctx).get_type(ctx))?;
        if !cases
            .iter()
            .any(|(_, payload)| admits_payload(ctx, self.get_result(ctx).get_type(ctx), *payload))
        {
            verify_err!(self.loc(ctx), InvalidVariant)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for VariantPayloadOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        true
    }
}
#[op_interface_impl]
impl MayThrowInterface for VariantPayloadOp {
    fn exception(&self, _: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
