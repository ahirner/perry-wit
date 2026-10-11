//! Keep unobserved character strings in scalar SSA form through comparison and append.
use crate::{
    aggregates::{functions, operations},
    collections::{StringAppendOp, StringConcatOp, StringConstOp, StringLenOp},
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
    if Operation::get_op::<StringConcatOp>(op, ctx).is_some()
        || Operation::get_op::<StringAppendOp>(op, ctx).is_some()
    {
        return operation.get_operand(1) == character && operation.get_operand(0) != character;
    }
    Operation::get_op::<StringLenOp>(op, ctx).is_some()
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
            .filter(|&op| {
                Operation::get_op::<IntrinsicOp>(op, ctx).and_then(|i| i.intrinsic(ctx))
                    == Some(Intrinsic::StringCharAt)
            })
            .collect();
        for op in candidates {
            let character = op.deref(ctx).get_result(0);
            let mut seen = HashSet::new();
            let mut users: Vec<_> = character
                .uses(ctx)
                .iter()
                .map(|usage| usage.user_op())
                .filter(|user| seen.insert(*user))
                .collect();
            if users.is_empty() || !users.iter().all(|&user| supports(ctx, user, character)) {
                continue;
            }
            users.sort_unstable_by_key(|user| order[user]);
            let args = op.deref(ctx).operands().collect();
            let scalar = IntrinsicOp::new(ctx, Intrinsic::StringScalarAt, args).get_operation();
            scalar.insert_before(ctx, op);
            let scalar = scalar.deref(ctx).get_result(0);
            for user in users {
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
                            constant.insert_before(ctx, user);
                            constant.deref(ctx).get_result(0)
                        })
                        .collect();
                    CompareOp::new(ctx, kind, args[0], args[1]).get_operation()
                } else if Operation::get_op::<StringLenOp>(user, ctx).is_some() {
                    IntrinsicOp::new(ctx, Intrinsic::StringScalarLength, vec![scalar])
                        .get_operation()
                } else {
                    let append = Operation::get_op::<StringAppendOp>(user, ctx).is_some();
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
