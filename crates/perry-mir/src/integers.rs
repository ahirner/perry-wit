//! Arbitrary-width integer magnitudes. Negation is represented as an ordinary `BigInt` operation.
use crate::operation_schema::op_schema;
use crate::signatures::Ty;
use pliron::{
    builtin::attributes::IntegerAttr, common_traits::Verify, context::Context, op::Op,
    operation::Operation, result::Result,
};

op_schema! {
    pub BigIntConstOp() -> infallible;
    name = "perry.bigint_const";
    format = "attr($perry_integer_value, $IntegerAttr) ` : ` type($0)";
    attributes = (perry_integer_value: IntegerAttr);
    constructors { pub new(magnitude: IntegerAttr) => infer(); }
    attribute(magnitude, get_attr_perry_integer_value, set_attr_perry_integer_value) = magnitude => InvalidConstant;
    result(ctx): Ty = Ty::BigInt;
    error = InvalidConstant;
    require magnitude.verify(ctx)?;
    check magnitude.get_type().deref(ctx).is_unsigned() => InvalidConstant;
}

impl BigIntConstOp {
    pub(crate) fn magnitude(&self, ctx: &Context) -> Option<u64> {
        let value = self.get_attr_perry_integer_value(ctx)?;
        (value.get_type().deref(ctx).width() <= 64).then(|| value.value().to_u64())
    }
}

#[derive(Debug)]
struct InvalidConstant;

impl core::fmt::Display for InvalidConstant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("bigint constant needs an unsigned magnitude and a bigint result")
    }
}

impl core::error::Error for InvalidConstant {}
