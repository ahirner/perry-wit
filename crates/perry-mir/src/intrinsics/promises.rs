//! Promise construction, combination and settled-value projections.
use super::*;

#[derive(Debug, Error)]
#[error("perry.promise_combinator takes an array of promises of one type")]
struct CombinatorErr;

/// How a [PromiseCombinatorOp] settles.
#[pliron::derive::pliron_attr(name = "perry.combinator", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Combinator {
    /// `Promise.all`: an array of every value, or the first rejection.
    All,
    /// `Promise.race`: the first settled input, in time and then input order.
    Race,
    /// `Promise.allSettled`: every input once all have settled, as `perry.settled` views.
    AllSettled,
}

/// `Promise.all`, `Promise.race` and `Promise.allSettled` of `perry.array<perry.promise<T>>`.
#[pliron_op(
    name = "perry.promise_combinator",
    format = "attr($perry_combinator, $Combinator) ` ` $0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface],
    attributes = (perry_combinator: Combinator)
)]
pub struct PromiseCombinatorOp;

impl PromiseCombinatorOp {
    /// `None` unless `promises` is an array of promises.
    pub fn new(ctx: &mut Context, combinator: Combinator, promises: Value) -> Option<Self> {
        use crate::types::PromiseType;
        let element =
            pliron::r#type::TypedHandle::<ArrayType>::from_handle(promises.get_type(ctx), ctx)
                .ok()?
                .deref(ctx)
                .element();
        let value = pliron::r#type::TypedHandle::<PromiseType>::from_handle(element, ctx)
            .ok()?
            .deref(ctx)
            .value();
        let settled = match combinator {
            Combinator::All => ArrayType::get(ctx, value).into(),
            Combinator::Race => value,
            Combinator::AllSettled => {
                let settled = crate::types::SettledType::get(ctx, value).into();
                ArrayType::get(ctx, settled).into()
            }
        };
        let result = PromiseType::get(ctx, settled).into();
        let op = PromiseCombinatorOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![promises],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_combinator(ctx, combinator);
        Some(op)
    }

    pub fn combinator(&self, ctx: &Context) -> Combinator {
        *self.get_attr_perry_combinator(ctx).expect("combinator")
    }
}

impl Verify for PromiseCombinatorOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        use crate::types::PromiseType;
        let operand = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        let ok = pliron::r#type::TypedHandle::<ArrayType>::from_handle(operand, ctx)
            .is_ok_and(|a| a.deref(ctx).element().deref(ctx).is::<PromiseType>());
        if !ok {
            verify_err!(self.loc(ctx), CombinatorErr)?
        }
        Ok(())
    }
}

/// A promise fulfilled with its operand: a plain value among a combinator's inputs
/// (`Promise.all([pending, 1])`).
#[pliron_op(
    name = "perry.promise_resolved",
    format = "$0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface]
)]
pub struct PromiseResolvedOp;

impl PromiseResolvedOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = value.get_type(ctx);
        let result = crate::types::PromiseType::get(ctx, ty).into();
        PromiseResolvedOp {
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

/// A promise rejected with its operand (`Promise.reject(e)`), of `perry.promise<T>` for the
/// settled type `T` its context expects.
#[pliron_op(
    name = "perry.promise_rejected",
    format = "$0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface]
)]
pub struct PromiseRejectedOp;

impl PromiseRejectedOp {
    pub fn new(ctx: &mut Context, reason: Value, settled: pliron::r#type::TypeHandle) -> Self {
        let result = crate::types::PromiseType::get(ctx, settled).into();
        PromiseRejectedOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![reason],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for PromiseRejectedOp {
    fn verify(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }
}

impl Verify for PromiseResolvedOp {
    fn verify(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Error)]
#[error("perry.settled_field reads a perry.settled value")]
struct SettledErr;

/// A property of a `Promise.allSettled` result element.
#[pliron::derive::pliron_attr(name = "perry.settled_field", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum SettledField {
    /// `"fulfilled"` or `"rejected"`.
    Status,
    /// The fulfillment value, or `undefined`.
    Value,
    /// The rejection (an exception), or `undefined`.
    Reason,
}

/// `settled.status`, `settled.value` or `settled.reason`.
#[pliron_op(
    name = "perry.settled_field",
    format = "attr($perry_settled_field, $SettledField) ` ` $0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface],
    attributes = (perry_settled_field: SettledField)
)]
pub struct SettledFieldOp;

impl SettledFieldOp {
    /// `None` unless `settled` is a `perry.settled` value.
    pub fn new(ctx: &mut Context, field: SettledField, settled: Value) -> Option<Self> {
        use crate::types::SettledType;
        let value =
            pliron::r#type::TypedHandle::<SettledType>::from_handle(settled.get_type(ctx), ctx)
                .ok()?
                .deref(ctx)
                .value();
        let result = match field {
            SettledField::Status => StringType::get(ctx).into(),
            SettledField::Value => OptionalType::get(ctx, value).into(),
            SettledField::Reason => {
                let exception = ExceptionType::get(ctx).into();
                OptionalType::get(ctx, exception).into()
            }
        };
        let op = SettledFieldOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![settled],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_settled_field(ctx, field);
        Some(op)
    }

    pub fn field(&self, ctx: &Context) -> SettledField {
        *self.get_attr_perry_settled_field(ctx).expect("field")
    }
}

impl Verify for SettledFieldOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let operand = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        if !operand.deref(ctx).is::<crate::types::SettledType>() {
            verify_err!(self.loc(ctx), SettledErr)?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for SettledFieldOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}
