//! Exact signed-magnitude integers for WIT's 64-bit transport domain.
use crate::operation_schema::op_schema;
use crate::signatures::Ty;
use pliron::{
    builtin::attributes::StringAttr, common_traits::Verify, context::Context, op::Op,
    operation::Operation, result::Result,
};

op_schema! {
    pub BigIntConstOp() -> infallible;
    name = "perry.bigint_const";
    format = "attr($perry_integer_value, $StringAttr) ` : ` type($0)";
    attributes = (perry_integer_value: StringAttr);
    constructors { pub new(magnitude: u64) => infer(); }
    attribute(_value, get_attr_perry_integer_value, set_attr_perry_integer_value) = StringAttr::new(magnitude.to_string());
    prepare(this) {}
    result(ctx): Ty = Ty::BigInt;
    error = InvalidConstant;
    check this.magnitude(ctx).is_some() => InvalidConstant;
}

impl BigIntConstOp {
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
