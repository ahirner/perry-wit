//! Scalar arithmetic and comparison.
use crate::attrs::{BinaryKind, CompareKind};
use crate::operation_schema::op_schema;
use crate::signatures::{Accepts, OutputContract, ResultType, Ty};
use pliron::{
    builtin::{
        op_interfaces::{
            NOpdsInterface, OneResultInterface, SameOperandsAndResultType, SameOperandsType,
            SameResultsType,
        },
        types::IntegerType,
    },
    common_traits::Verify,
    context::Context,
    derive::pliron_op,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
};

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

pliron::dict_key!(UNCONSTRAINED_ARITH, "perry_unconstrained_arith");

impl BinaryOp {
    pub fn new(ctx: &mut Context, kind: BinaryKind, lhs: Value, rhs: Value) -> Self {
        let ty = lhs.get_type(ctx);
        let op = Operation::new_op::<Self>(ctx, vec![ty], vec![lhs, rhs], vec![], 0);
        op.set_attr_perry_binary_kind(ctx, kind);
        op
    }

    pub fn kind(&self, ctx: &Context) -> BinaryKind {
        *self.get_attr_perry_binary_kind(ctx).expect("binary kind")
    }

    /// Marks numeric add/sub/mul whose exact result no integer consumer constrains.
    pub(crate) fn set_unconstrained(&self, ctx: &mut Context, unconstrained: bool) {
        if unconstrained {
            self.get_operation().deref_mut(ctx).attributes.set(
                UNCONSTRAINED_ARITH.clone(),
                pliron::builtin::attributes::UnitAttr::new(),
            );
        } else {
            self.get_operation()
                .deref_mut(ctx)
                .attributes
                .0
                .remove(&UNCONSTRAINED_ARITH);
        }
    }

    pub fn is_unconstrained(&self, ctx: &Context) -> bool {
        self.get_operation()
            .deref(ctx)
            .attributes
            .0
            .contains_key(&UNCONSTRAINED_ARITH)
    }
}

op_schema! {
    /// Comparison of two operands of one type, producing an `i1`.
    ///
    /// On `perry.optional` operands `Eq` and `Ne` implement ECMAScript strict equality, where
    /// `undefined` equals only `undefined`. The builder coerces operands with `perry.to_number`
    /// before ordered comparisons.
    pub CompareOp(_lhs, _rhs) -> infallible;
    name = "perry.compare";
    format = "attr($perry_compare_kind, $CompareKind) ` ` $0 `, ` $1 ` : ` type($0)";
    interfaces = [SameOperandsType];
    attributes = (perry_compare_kind: CompareKind);
    constructors { pub new(kind: CompareKind, _lhs: Value, _rhs: Value) => infer(); }
    attribute(kind: CompareKind, get_attr_perry_compare_kind, set_attr_perry_compare_kind) = kind;
    result(ctx): Ty = Ty::Predicate;
    error = ScalarErr::CompareResult;
}

/// Whether `value` is a signless `i1`, the type of branch conditions.
pub(crate) fn is_bool(ctx: &Context, value: Value) -> bool {
    Ty::Predicate.matches(ctx, value.get_type(ctx))
}

op_schema! {
    /// Representation conversion with explicit source and destination numeric types.
    /// Integer narrowing wraps, and integer widening uses the source signedness. Floating-point
    /// conversion to an integer saturates, matching the floating-point WIT boundary.
    pub NumericCastOp(value) -> infallible;
    name = "perry.numeric_cast";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value, target: TypeHandle) => explicit(target); }
    result(ctx, value: Value): impl OutputContract = {
        Accepts(move |ctx: &Context, target: TypeHandle| {
            let source = value.get_type(ctx);
            let bigint = |ty: TypeHandle| ty.deref(ctx).is::<crate::types::BigIntType>();
            let integer = |ty: TypeHandle| {
                ty.deref(ctx)
                    .downcast_ref::<IntegerType>()
                    .is_some_and(|ty| matches!(ty.width(), 8 | 16 | 32 | 64))
            };
            if bigint(source) || bigint(target) {
                return integer(if bigint(source) { target } else { source });
            }
            [source, target].into_iter().all(|ty| {
                let ty = crate::optional::present_type(ctx, ty);
                ty.deref(ctx).is::<crate::types::NumericType>()
                    || Ty::Number.matches(ctx, ty)
                    || integer(ty)
            })
        })
    };
    error = ScalarErr::NumericCast;
}

#[derive(Debug)]
enum ScalarErr {
    CompareResult,
    NumericCast,
}

impl core::fmt::Display for ScalarErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CompareResult => f.write_str("perry.compare must produce a signless i1"),
            Self::NumericCast => f.write_str(
                "perry.numeric_cast requires numeric conversion or integer/bigint boxing",
            ),
        }
    }
}

impl core::error::Error for ScalarErr {}
