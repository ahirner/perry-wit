//! Keep unobserved character strings in scalar SSA form through comparison and append.
use crate::{
    aggregates::operations,
    collections::{ConcatOp, ConcatReuseOp, StringConstOp, StringLenOp},
    intrinsics::{Intrinsic, IntrinsicOp},
    scalar::CompareOp,
};
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        ops::{ConstantOp, FuncOp},
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    irbuild::{
        inserter::Inserter,
        match_rewrite::{MatchRewrite, MatchRewriter},
        rewriter::Rewriter,
    },
    op::Op,
    operation::Operation,
    result::Result,
    utils::apint::{APInt, bw},
    value::Value,
};
use std::collections::HashSet;

/// A scalar or the empty string. UTF-8 ordering agrees with scalar ordering; -1 precedes NUL.
fn literal(ctx: &Context, value: Value) -> Option<i32> {
    let constant = Operation::get_op::<StringConstOp>(value.defining_op()?, ctx)?;
    let text = constant.get_attr_perry_string_value(ctx)?;
    let text: &str = text.as_ref();
    let mut chars = text.chars();
    let scalar = chars.next().map_or(-1, |c| c as i32);
    chars.next().is_none().then_some(scalar)
}

fn supports(ctx: &Context, op: Ptr<Operation>, character: Value) -> bool {
    let operation = op.deref(ctx);
    if Operation::get_op::<CompareOp>(op, ctx).is_some() {
        return operation
            .operands()
            .all(|value| value == character || literal(ctx, value).is_some());
    }
    if Operation::get_op::<ConcatOp>(op, ctx).is_some()
        || Operation::get_op::<ConcatReuseOp>(op, ctx).is_some()
    {
        return operation.get_operand(1) == character && operation.get_operand(0) != character;
    }
    Operation::get_op::<StringLenOp>(op, ctx).is_some()
}

/// Materialized characters that escape, enter unknown calls or join other string values
/// retain their ordinary representation. The existing append pass supplies ownership proof.
pub(crate) struct ScalarizeCharacters;

impl MatchRewrite for ScalarizeCharacters {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        if !matches!(
            Operation::get_op::<IntrinsicOp>(op, ctx).and_then(|i| i.intrinsic(ctx)),
            Some(Intrinsic::StringCharAt | Intrinsic::StringCharAtIndex)
        ) {
            return false;
        }
        let character = op.deref(ctx).get_result(0);
        let users = character.uses(ctx);
        !users.is_empty()
            && users
                .iter()
                .all(|usage| supports(ctx, usage.user_op(), character))
    }

    fn rewrite(
        &mut self,
        ctx: &mut Context,
        rewriter: &mut MatchRewriter,
        op: Ptr<Operation>,
    ) -> Result<()> {
        if !self.r#match(ctx, op) {
            return Ok(());
        }
        let kind = match Operation::get_op::<IntrinsicOp>(op, ctx).and_then(|i| i.intrinsic(ctx)) {
            Some(Intrinsic::StringCharAt) => Intrinsic::StringScalarAt,
            Some(Intrinsic::StringCharAtIndex) => Intrinsic::StringScalarAtIndex,
            _ => unreachable!("matched character lookup"),
        };
        let character = op.deref(ctx).get_result(0);
        let users: HashSet<_> = character
            .uses(ctx)
            .iter()
            .map(|usage| usage.user_op())
            .collect();
        let function = Operation::get_op::<FuncOp>(
            op.deref(ctx)
                .get_parent_op(ctx)
                .expect("character in function"),
            ctx,
        )
        .expect("MIR function");
        let users: Vec<_> = operations(ctx, &function)
            .into_iter()
            .filter(|user| users.contains(user))
            .collect();
        let args = op.deref(ctx).operands().collect();
        let scalar = IntrinsicOp::new(ctx, kind, args).get_operation();
        rewriter.insert_operation(ctx, scalar);
        let scalar = scalar.deref(ctx).get_result(0);
        for user in users {
            rewriter.set_insertion_point_before_operation(user);
            let replacement = if let Some(compare) = Operation::get_op::<CompareOp>(user, ctx) {
                let kind = compare.kind(ctx);
                let args: Vec<_> = user.deref(ctx).operands().collect();
                let args: Vec<_> = args
                    .into_iter()
                    .map(|value| {
                        if value == character {
                            return scalar;
                        }
                        let value = literal(ctx, value).expect("validated character literal");
                        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
                        let attr =
                            IntegerAttr::new(ty, APInt::from_u64(value as u32 as u64, bw(32)));
                        let constant = ConstantOp::new(ctx, Box::new(attr)).get_operation();
                        rewriter.insert_operation(ctx, constant);
                        constant.deref(ctx).get_result(0)
                    })
                    .collect();
                CompareOp::new(ctx, kind, args[0], args[1]).get_operation()
            } else if Operation::get_op::<StringLenOp>(user, ctx).is_some() {
                IntrinsicOp::new(ctx, Intrinsic::StringScalarLength, vec![scalar]).get_operation()
            } else {
                let append = Operation::get_op::<ConcatReuseOp>(user, ctx).is_some();
                let prefix = user.deref(ctx).get_operand(0);
                IntrinsicOp::new(
                    ctx,
                    if append {
                        Intrinsic::StringScalarAppend
                    } else {
                        Intrinsic::StringScalarConcat
                    },
                    vec![prefix, scalar],
                )
                .get_operation()
            };
            rewriter.insert_operation(ctx, replacement);
            rewriter.replace_operation(ctx, user, replacement);
        }
        rewriter.erase_operation(ctx, op);
        Ok(())
    }
}
