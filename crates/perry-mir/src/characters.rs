//! Keep unobserved character strings in scalar SSA form through comparison and append.
use crate::{
    aggregates::{functions, operations},
    collections::{ConcatOp, ConcatReuseOp, StringConstOp, StringLenOp},
    intrinsics::{Intrinsic, IntrinsicOp},
    scalar::CompareOp,
};
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        ops::ConstantOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    irbuild::IRStatus,
    op::Op,
    operation::Operation,
    utils::apint::{APInt, bw},
    value::Value,
};
use std::collections::{HashMap, HashSet};

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
    user: Ptr<Operation>,
    scalar: Value,
    operand: CompareOperand,
) -> Value {
    match operand {
        CompareOperand::Character => scalar,
        CompareOperand::Literal(value) => {
            let ty = IntegerType::get(ctx, 32, Signedness::Signless);
            let attr = IntegerAttr::new(ty, APInt::from_u64(value as u32 as u64, bw(32)));
            let constant = ConstantOp::new(ctx, Box::new(attr)).get_operation();
            constant.insert_before(ctx, user);
            constant.deref(ctx).get_result(0)
        }
    }
}

/// Materialized characters that escape, enter unknown calls or join other string values
/// retain their ordinary representation. The existing append pass supplies ownership proof.
pub(crate) fn scalar_replace(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    let mut status = IRStatus::Unchanged;
    for function in functions(root, ctx) {
        let body = operations(ctx, &function);
        let mut order: HashMap<_, _> = body.iter().enumerate().map(|(i, &op)| (op, i)).collect();
        let candidates: Vec<_> = body
            .into_iter()
            .filter_map(
                |op| match Operation::get_op::<IntrinsicOp>(op, ctx)?.intrinsic(ctx)? {
                    Intrinsic::StringCharAt => Some((op, Intrinsic::StringScalarAt)),
                    Intrinsic::StringCharAtIndex => Some((op, Intrinsic::StringScalarAtIndex)),
                    _ => None,
                },
            )
            .collect();
        for (op, kind) in candidates {
            let character = op.deref(ctx).get_result(0);
            let mut seen = HashSet::new();
            let Some(mut users) = character
                .uses(ctx)
                .iter()
                .map(|usage| usage.user_op())
                .filter(|user| seen.insert(*user))
                .map(|user| Some((user, classify_user(ctx, user, character)?)))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            if users.is_empty() {
                continue;
            }
            users.sort_unstable_by_key(|(user, _)| order[user]);
            let args = op.deref(ctx).operands().collect();
            let scalar = IntrinsicOp::new(ctx, kind, args).get_operation();
            scalar.insert_before(ctx, op);
            let scalar = scalar.deref(ctx).get_result(0);
            for (user, action) in users {
                let replacement = match action {
                    UserAction::Compare { kind, lhs, rhs } => {
                        let lhs = lower_compare_operand(ctx, user, scalar, lhs);
                        let rhs = lower_compare_operand(ctx, user, scalar, rhs);
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
                replacement.insert_before(ctx, user);
                let old = user.deref(ctx).get_result(0);
                let new = replacement.deref(ctx).get_result(0);
                old.replace_all_uses_with(ctx, &new);
                Operation::erase(user, ctx);
            }
            Operation::erase(op, ctx);
            status = IRStatus::Changed;
        }
    }
    status
}
