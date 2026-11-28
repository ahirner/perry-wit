//! Optional-value and exact-conversion canonicalization.
use crate::{
    attrs::CompareKind,
    exceptions::CaughtOp,
    optional::{IsUndefinedOp, OmittedOp, ToNumberOp, ToStringOp, UndefinedOp, UnwrapOp, WrapOp},
    scalar::{CompareOp, NumericCastOp},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{BranchOpInterface, OneRegionInterface},
        types::IntegerType,
    },
    context::{Context, Ptr},
    irbuild::{
        IRStatus,
        inserter::Inserter,
        match_rewrite::{MatchRewrite, MatchRewriter},
        rewriter::Rewriter,
    },
    linked_list::{ContainsLinkedList, LinkedList},
    op::{Op, op_cast},
    operation::Operation,
    result::Result,
    r#type::Typed,
    value::Value,
};

fn wrapped(ctx: &Context, value: Value) -> Option<Value> {
    let op = value.defining_op()?;
    Operation::get_op::<WrapOp>(op, ctx)?;
    Some(op.deref(ctx).get_operand(0))
}

/// Simplify arguments at control-flow joins, preserving entry-block function parameters.
pub(crate) fn simplify_block_arguments(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    let mut status = IRStatus::Unchanged;
    for function in crate::aggregates::functions(root, ctx) {
        let blocks: Vec<_> = function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
            .skip(1)
            .collect();
        for block in blocks {
            let arguments: Vec<_> = block.deref(ctx).arguments().collect();
            for (index, argument) in arguments.into_iter().enumerate().rev() {
                if argument.is_used(ctx) {
                    status |= narrow_wrapped_argument(ctx, block, index, argument);
                    continue;
                }
                for predecessor in block.uses(ctx) {
                    let successor = predecessor.find_index(ctx);
                    let branch = Operation::get_op_dyn(predecessor.user_op(), ctx);
                    op_cast::<dyn BranchOpInterface>(&*branch)
                        .expect("MIR branch")
                        .remove_successor_operand(ctx, successor, index);
                }
                BasicBlock::remove_argument(block, ctx, index);
                status = IRStatus::Changed;
            }
        }
    }
    status
}

/// Carry the payload through a join when every incoming edge wraps a present value
/// or forwards the same argument unchanged on a backedge.
fn narrow_wrapped_argument(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    index: usize,
    argument: Value,
) -> IRStatus {
    let optional = argument.get_type(ctx);
    let Some(payload) = crate::optional_value(ctx, optional) else {
        return IRStatus::Unchanged;
    };
    let mut incoming = Vec::new();
    for edge in block.uses(ctx) {
        let terminator = edge.user_op();
        let dynamic = Operation::get_op_dyn(terminator, ctx);
        let branch = op_cast::<dyn BranchOpInterface>(&*dynamic).expect("MIR branch");
        let position = branch
            .successor_operand_range(ctx, edge.find_index(ctx))
            .start
            + index;
        let value = terminator.deref(ctx).get_operand(position);
        let inner = if value == argument {
            argument
        } else if let Some(inner) = wrapped(ctx, value) {
            inner
        } else {
            return IRStatus::Unchanged;
        };
        incoming.push((terminator, position, inner));
    }
    if !incoming.iter().any(|&(_, _, value)| value != argument) {
        return IRStatus::Unchanged;
    }

    argument.set_type(ctx, payload);
    let wrap = WrapOp::with_type(ctx, argument, optional).get_operation();
    wrap.insert_at_front(block, ctx);
    let replacement = wrap.deref(ctx).get_result(0);
    argument.replace_some_uses_with(ctx, |_, usage| usage.user_op() != wrap, &replacement);
    for (terminator, position, value) in incoming {
        Operation::replace_operand(terminator, ctx, position, value);
    }
    IRStatus::Changed
}

fn converted_integer(ctx: &Context, value: Value) -> Option<Value> {
    let op = value.defining_op()?;
    Operation::get_op::<NumericCastOp>(op, ctx)?;
    let input = op.deref(ctx).get_operand(0);
    let val_ty = value.get_type(ctx);
    let in_ty = input.get_type(ctx);
    let is_f64_i32 = val_ty.deref(ctx).is::<pliron::builtin::types::FP64Type>()
        && in_ty
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() <= 32);
    let is_bigint_i64 = val_ty.deref(ctx).is::<crate::types::BigIntType>()
        && in_ty
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() == 64);
    (is_f64_i32 || is_bigint_i64).then_some(input)
}

enum Replacement {
    Value(Value),
    Cast(Value, pliron::r#type::TypeHandle),
    Bool(bool),
    Compare(CompareKind, Value, Value),
}

fn replacement(ctx: &Context, op: Ptr<Operation>) -> Option<Replacement> {
    let operation = op.deref(ctx);
    if Operation::get_op::<IsUndefinedOp>(op, ctx).is_some() {
        let operand = operation.get_operand(0);
        if wrapped(ctx, operand).is_some() {
            return Some(Replacement::Bool(false));
        }
        let undefined = operand.defining_op().is_some_and(|def| {
            Operation::get_op::<UndefinedOp>(def, ctx).is_some()
                || Operation::get_op::<OmittedOp>(def, ctx).is_some()
        });
        return undefined.then_some(Replacement::Bool(true));
    }
    if Operation::get_op::<ToNumberOp>(op, ctx).is_some()
        || Operation::get_op::<ToStringOp>(op, ctx).is_some()
    {
        let inner = wrapped(ctx, operation.get_operand(0))?;
        return (inner.get_type(ctx) == operation.get_result(0).get_type(ctx))
            .then_some(Replacement::Value(inner));
    }
    if Operation::get_op::<UnwrapOp>(op, ctx).is_some() {
        let caught = operation
            .get_next()
            .is_some_and(|next| Operation::get_op::<CaughtOp>(next, ctx).is_some());
        return (!caught)
            .then(|| wrapped(ctx, operation.get_operand(0)))
            .flatten()
            .map(Replacement::Value);
    }
    if let Some(compare) = Operation::get_op::<CompareOp>(op, ctx) {
        let lhs = converted_integer(ctx, operation.get_operand(0))?;
        let rhs = converted_integer(ctx, operation.get_operand(1))?;
        return (lhs.get_type(ctx) == rhs.get_type(ctx)).then_some(Replacement::Compare(
            compare.kind(ctx),
            lhs,
            rhs,
        ));
    }
    if Operation::get_op::<NumericCastOp>(op, ctx).is_some() {
        let operand = operation.get_operand(0);
        let target = operation.get_result(0).get_type(ctx);
        if operand.get_type(ctx) == target {
            return Some(Replacement::Value(operand));
        }
        if operand
            .get_type(ctx)
            .deref(ctx)
            .is::<crate::types::NumericType>()
            && crate::optional::optional_value(ctx, target).is_none()
        {
            let inner = operand.defining_op()?;
            Operation::get_op::<NumericCastOp>(inner, ctx)?;
            let input = inner.deref(ctx).get_operand(0);
            if crate::optional::optional_value(ctx, input.get_type(ctx)).is_none() {
                return Some(if input.get_type(ctx) == target {
                    Replacement::Value(input)
                } else {
                    Replacement::Cast(input, target)
                });
            }
        }
        if let Some(inner) = converted_integer(ctx, operand) {
            if inner.get_type(ctx) == target {
                return Some(Replacement::Value(inner));
            }
            if target.deref(ctx).is::<IntegerType>() {
                return Some(Replacement::Cast(inner, target));
            }
        }
    }
    None
}

pub(crate) struct Canonicalize;

impl MatchRewrite for Canonicalize {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        replacement(ctx, op).is_some()
    }

    fn rewrite(
        &mut self,
        ctx: &mut Context,
        rewriter: &mut MatchRewriter,
        op: Ptr<Operation>,
    ) -> Result<()> {
        let Some(replacement) = replacement(ctx, op) else {
            return Ok(());
        };
        let value = match replacement {
            Replacement::Value(value) => value,
            Replacement::Cast(inner, target) => {
                let cast = NumericCastOp::new(ctx, inner, target).get_operation();
                rewriter.insert_operation(ctx, cast);
                cast.deref(ctx).get_result(0)
            }
            Replacement::Bool(flag) => {
                let constant =
                    crate::constants::integer_constant(ctx, 1, u64::from(flag)).get_operation();
                rewriter.insert_operation(ctx, constant);
                constant.deref(ctx).get_result(0)
            }
            Replacement::Compare(kind, lhs, rhs) => {
                let compare = CompareOp::new(ctx, kind, lhs, rhs).get_operation();
                rewriter.insert_operation(ctx, compare);
                compare.deref(ctx).get_result(0)
            }
        };
        let result = op.deref(ctx).get_result(0);
        rewriter.replace_value_uses_with(ctx, result, value);
        rewriter.erase_operation(ctx, op);
        Ok(())
    }
}
