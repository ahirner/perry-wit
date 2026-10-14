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
use std::collections::HashMap;

#[derive(Clone, Copy)]
enum CompareOperand {
    Character,
    Literal(i32),
}

enum UserAction {
    Compare {
        kind: crate::attrs::CompareKind,
        lhs: CompareOperand,
        rhs: CompareOperand,
    },
    Length,
    Concat {
        prefix: Value,
        append: bool,
    },
}

/// A scalar or the empty string. UTF-8 ordering agrees with scalar ordering; -1 precedes NUL.
fn literal(ctx: &Context, value: Value) -> Option<i32> {
    let constant = Operation::get_op::<StringConstOp>(value.defining_op()?, ctx)?;
    let text = constant.get_attr_perry_string_value(ctx)?;
    let text: &str = text.as_ref();
    let mut chars = text.chars();
    let scalar = chars.next().map_or(-1, |c| c as i32);
    chars.next().is_none().then_some(scalar)
}

fn classify_user(ctx: &Context, op: Ptr<Operation>, character: Value) -> Option<UserAction> {
    let operation = op.deref(ctx);
    if let Some(compare) = Operation::get_op::<CompareOp>(op, ctx) {
        let operand = |value: Value| {
            if value == character {
                Some(CompareOperand::Character)
            } else {
                literal(ctx, value).map(CompareOperand::Literal)
            }
        };
        let lhs = operand(operation.get_operand(0))?;
        let rhs = operand(operation.get_operand(1))?;
        drop(operation);
        return Some(UserAction::Compare {
            kind: compare.kind(ctx),
            lhs,
            rhs,
        });
    }
    if Operation::get_op::<ConcatOp>(op, ctx).is_some()
        || Operation::get_op::<ConcatReuseOp>(op, ctx).is_some()
    {
        let prefix = operation.get_operand(0);
        let suffix = operation.get_operand(1);
        drop(operation);
        return (suffix == character && prefix != character).then(|| UserAction::Concat {
            prefix,
            append: Operation::get_op::<ConcatReuseOp>(op, ctx).is_some(),
        });
    }
    drop(operation);
    Operation::get_op::<StringLenOp>(op, ctx)
        .is_some()
        .then_some(UserAction::Length)
}

fn lower_compare_operand(
    ctx: &mut Context,
    rewriter: &mut MatchRewriter,
    scalar: Value,
    operand: CompareOperand,
) -> Value {
    match operand {
        CompareOperand::Character => scalar,
        CompareOperand::Literal(value) => {
            let ty = IntegerType::get(ctx, 32, Signedness::Signless);
            let attr = IntegerAttr::new(ty, APInt::from_u64(value as u32 as u64, bw(32)));
            let constant = ConstantOp::new(ctx, Box::new(attr)).get_operation();
            rewriter.insert_operation(ctx, constant);
            constant.deref(ctx).get_result(0)
        }
    }
}

/// Materialized characters that escape, enter unknown calls or join other string values
/// retain their ordinary representation. The existing append pass supplies ownership proof.
#[derive(Default)]
pub(crate) struct ScalarizeCharacters {
    order: HashMap<Ptr<Operation>, HashMap<Ptr<Operation>, usize>>,
}

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
                .all(|usage| classify_user(ctx, usage.user_op(), character).is_some())
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
        let mut users: Vec<_> = character
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
        let order = self
            .order
            .entry(function.get_operation())
            .or_insert_with(|| {
                operations(ctx, &function)
                    .into_iter()
                    .enumerate()
                    .map(|(index, op)| (op, index))
                    .collect()
            });
        users.sort_unstable_by_key(|user| order[user]);
        users.dedup();
        let args = op.deref(ctx).operands().collect();
        let scalar = IntrinsicOp::new(ctx, kind, args).get_operation();
        rewriter.insert_operation(ctx, scalar);
        let scalar = scalar.deref(ctx).get_result(0);
        for user in users {
            let action = classify_user(ctx, user, character).expect("validated character user");
            rewriter.set_insertion_point_before_operation(user);
            let replacement = match action {
                UserAction::Compare { kind, lhs, rhs } => {
                    let lhs = lower_compare_operand(ctx, rewriter, scalar, lhs);
                    let rhs = lower_compare_operand(ctx, rewriter, scalar, rhs);
                    CompareOp::new(ctx, kind, lhs, rhs).get_operation()
                }
                UserAction::Length => {
                    IntrinsicOp::new(ctx, Intrinsic::StringScalarLength, vec![scalar])
                        .get_operation()
                }
                UserAction::Concat { prefix, append } => IntrinsicOp::new(
                    ctx,
                    if append {
                        Intrinsic::StringScalarAppend
                    } else {
                        Intrinsic::StringScalarConcat
                    },
                    vec![prefix, scalar],
                )
                .get_operation(),
            };
            order.insert(replacement, order[&user]);
            rewriter.insert_operation(ctx, replacement);
            rewriter.replace_operation(ctx, user, replacement);
        }
        rewriter.erase_operation(ctx, op);
        Ok(())
    }
}
