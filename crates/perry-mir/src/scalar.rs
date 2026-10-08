//! Scalar arithmetic and comparison.
use crate::attrs::{BinaryKind, CompareKind};
use pliron::{
    builtin::{
        op_interfaces::{
            NOpdsInterface, OneResultInterface, SameOperandsAndResultType, SameOperandsType,
            SameResultsType,
        },
        types::{IntegerType, Signedness},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::Typed,
    value::Value,
    verify_err,
};
use thiserror::Error;

/// Binary arithmetic over operands and result of one scalar type.
#[pliron_op(
    name = "perry.binary",
    format = "attr($perry_binary_kind, $BinaryKind) ` ` $0 `, ` $1 ` : ` type($0)",
    interfaces = [
        NOpdsInterface<2>,
        OneResultInterface,
        SameOperandsType,
        SameResultsType,
        SameOperandsAndResultType
    ],
    attributes = (perry_binary_kind: BinaryKind),
    verifier = "succ"
)]
pub struct BinaryOp;

impl BinaryOp {
    pub fn new(ctx: &mut Context, kind: BinaryKind, lhs: Value, rhs: Value) -> Self {
        let ty = lhs.get_type(ctx);
        let op = BinaryOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![lhs, rhs],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_binary_kind(ctx, kind);
        op
    }

    pub fn kind(&self, ctx: &Context) -> BinaryKind {
        *self.get_attr_perry_binary_kind(ctx).expect("binary kind")
    }
}

#[op_interface_impl]
impl SideEffects for BinaryOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Comparison of two operands of one scalar type, producing an `i1`.
#[pliron_op(
    name = "perry.compare",
    format = "attr($perry_compare_kind, $CompareKind) ` ` $0 `, ` $1 ` : ` type($0)",
    interfaces = [NOpdsInterface<2>, OneResultInterface, SameOperandsType],
    attributes = (perry_compare_kind: CompareKind)
)]
pub struct CompareOp;

impl CompareOp {
    pub fn new(ctx: &mut Context, kind: CompareKind, lhs: Value, rhs: Value) -> Self {
        let i1 = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let op = CompareOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![i1],
                vec![lhs, rhs],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_compare_kind(ctx, kind);
        op
    }

    pub fn kind(&self, ctx: &Context) -> CompareKind {
        *self.get_attr_perry_compare_kind(ctx).expect("compare kind")
    }
}

#[derive(Debug, Error)]
#[error("perry.compare must produce a signless i1")]
pub struct CompareResultErr;

impl Verify for CompareOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_bool(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), CompareResultErr)?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for CompareOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Whether `value` is a signless `i1`, the type of branch conditions.
pub fn is_bool(ctx: &Context, value: Value) -> bool {
    value
        .get_type(ctx)
        .deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|ty| ty.width() == 1 && ty.signedness() == Signedness::Signless)
}
