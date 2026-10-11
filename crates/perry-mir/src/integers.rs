//! Exact signed-magnitude integers for WIT's 64-bit transport domain.
use crate::types::BigIntType;
use pliron::{
    builtin::{
        attributes::StringAttr,
        op_interfaces::{NOpdsInterface, OneResultInterface},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::Typed,
    verify_err,
};

#[pliron_op(name = "perry.bigint_const",
    format = "attr($perry_integer_value, $StringAttr) ` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface],
    attributes = (perry_integer_value: StringAttr))]
pub struct BigIntConstOp;

impl BigIntConstOp {
    pub fn new(ctx: &mut Context, magnitude: u64) -> Self {
        let ty = BigIntType::get(ctx).into();
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_integer_value(ctx, StringAttr::new(magnitude.to_string()));
        op
    }

    pub fn magnitude(&self, ctx: &Context) -> Option<u64> {
        self.get_attr_perry_integer_value(ctx)?
            .as_str()
            .parse()
            .ok()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("bigint constant needs a 64-bit magnitude and a bigint result")]
struct InvalidConstant;

impl Verify for BigIntConstOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if self.magnitude(ctx).is_none()
            || !self
                .get_result(ctx)
                .get_type(ctx)
                .deref(ctx)
                .is::<BigIntType>()
        {
            verify_err!(self.loc(ctx), InvalidConstant)?;
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for BigIntConstOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}
