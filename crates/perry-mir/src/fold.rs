//! Optional-value and exact-conversion canonicalization.
use crate::{
    attrs::CompareKind,
    exceptions::CaughtOp,
    optional::{IsUndefinedOp, ToNumberOp, ToStringOp, UndefinedOp, UnwrapOp, WrapOp},
    scalar::{CompareOp, NumericCastOp},
};
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        ops::ConstantOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    irbuild::{
        inserter::Inserter,
        match_rewrite::{MatchRewrite, MatchRewriter},
        rewriter::Rewriter,
    },
    linked_list::LinkedList,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::Typed,
    utils::apint::{APInt, bw},
    value::Value,
};

fn wrapped(ctx: &Context, value: Value) -> Option<Value> {
    let op = value.defining_op()?;
    Operation::get_op::<WrapOp>(op, ctx)?;
    Some(op.deref(ctx).get_operand(0))
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
        let undefined = operand
            .defining_op()
            .is_some_and(|def| Operation::get_op::<UndefinedOp>(def, ctx).is_some());
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
                let ty = IntegerType::get(ctx, 1, Signedness::Signless);
                let attr = IntegerAttr::new(ty, APInt::from_u64(u64::from(flag), bw(1)));
                let constant = ConstantOp::new(ctx, Box::new(attr)).get_operation();
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
