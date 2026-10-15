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
    r#type::{TypeHandle, Typed},
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

/// Comparison of two operands of one type, producing an `i1`.
///
/// On `perry.optional` operands `Eq`/`Ne` are ECMAScript strict equality (`undefined` equals
/// only `undefined`); the builder coerces with `perry.to_number` before ordered comparisons.
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
pub(crate) struct CompareResultErr;

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
pub(crate) fn is_bool(ctx: &Context, value: Value) -> bool {
    value
        .get_type(ctx)
        .deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|ty| ty.width() == 1 && ty.signedness() == Signedness::Signless)
}

/// A representation conversion with explicit source and destination numeric types.
/// Integer narrowing wraps; integer widening uses the source signedness. Floating-point
/// conversion to an integer saturates, matching the existing floating-point WIT boundary.
#[pliron_op(
    name = "perry.numeric_cast",
    format = "$0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, OneResultInterface]
)]
pub struct NumericCastOp;

impl NumericCastOp {
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

#[derive(Debug, Error)]
#[error("perry.numeric_cast requires numeric conversion or integer/bigint boxing")]
struct NumericCastErr;

impl Verify for NumericCastOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        let source = op.get_operand(0).get_type(ctx);
        let target = op.get_result(0).get_type(ctx);
        if source.deref(ctx).is::<crate::types::BigIntType>()
            || target.deref(ctx).is::<crate::types::BigIntType>()
        {
            let other = if source.deref(ctx).is::<crate::types::BigIntType>() {
                target
            } else {
                source
            };
            if other
                .deref(ctx)
                .downcast_ref::<IntegerType>()
                .is_some_and(|ty| matches!(ty.width(), 8 | 16 | 32 | 64))
            {
                return Ok(());
            }
            verify_err!(self.loc(ctx), NumericCastErr)?;
        }
        for value in op.operands().chain(op.results()) {
            let ty = value.get_type(ctx);
            let ty = ty.deref(ctx);
            if !ty.is::<crate::types::NumericType>()
                && !ty.is::<pliron::builtin::types::FP64Type>()
                && !ty
                    .downcast_ref::<IntegerType>()
                    .is_some_and(|ty| matches!(ty.width(), 8 | 16 | 32 | 64))
            {
                verify_err!(self.loc(ctx), NumericCastErr)?;
            }
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for NumericCastOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}
