//! Bounded frame storage for closed scalar records with no escaping aliases.
use crate::{
    aggregates::{functions, operations},
    attrs::Tier,
    heap::{AllocOp, FieldGetOp, FieldPresentOp, FieldSetOp},
};
use pliron::{
    builtin::{op_interfaces::OneResultInterface, ops::FuncOp},
    context::{Context, Ptr},
    irbuild::IRStatus,
    op::Op,
    operation::Operation,
    r#type::{Typed, TypedHandle},
};

/// A fixed small frame permits iteration reuse only when the allocation's SSA value never
/// flows through a join, a store or a retaining callee. Reference fields remain collectible
/// until their longer-lived children and mutation effects have stronger proofs.
fn frame_candidates(
    ctx: &Context,
    function: &FuncOp,
    summaries: &crate::facts::ProgramFacts,
) -> Vec<AllocOp> {
    if crate::calls::is_async(ctx, function) {
        return vec![];
    }
    let mut bytes = 0;
    operations(ctx, function)
        .into_iter()
        .filter_map(|op| Operation::get_op::<AllocOp>(op, ctx))
        .filter(|allocation| {
            let value = allocation.get_result(ctx);
            let record =
                TypedHandle::<crate::types::RecordType>::from_handle(value.get_type(ctx), ctx)
                    .expect("verified record");
            let record = record.deref(ctx);
            let fields = record.fields().expect("verified fields");
            let payload_bytes: usize = fields
                .iter()
                .map(|(_, ty)| {
                    if crate::optional::present_type(ctx, *ty)
                        .deref(ctx)
                        .is::<crate::types::NumericType>()
                    {
                        16
                    } else {
                        8
                    }
                })
                .sum();
            let size = 8 + payload_bytes;
            if bytes + size > 448
                || fields
                    .iter()
                    .any(|(_, ty)| crate::types::is_reference(ctx, *ty))
            {
                return false;
            }
            let local = value.uses(ctx).iter().all(|usage| {
                let op = usage.user_op();
                let operand = usage.find_index(ctx);
                if operand == 0
                    && crate::is_any::<(FieldGetOp, FieldPresentOp, FieldSetOp)>(ctx, op)
                {
                    return true;
                }
                Operation::get_op::<crate::calls::CallOp>(op, ctx)
                    .and_then(|call| summaries.effects(&call.callee(ctx)))
                    .is_some_and(|summary| !summary.escapes.contains(&operand))
            });
            if local {
                bytes += size;
            }
            local
        })
        .collect()
}

pub(crate) fn assign(
    root: Ptr<Operation>,
    ctx: &Context,
    summaries: &crate::facts::ProgramFacts,
) -> IRStatus {
    let mut status = IRStatus::Unchanged;
    for function in functions(root, ctx) {
        let frame: std::collections::HashSet<_> = frame_candidates(ctx, &function, summaries)
            .into_iter()
            .map(|allocation| allocation.get_operation())
            .collect();
        for op in operations(ctx, &function) {
            if let Some(allocation) = Operation::get_op::<AllocOp>(op, ctx) {
                let tier = if frame.contains(&op) {
                    Tier::Frame
                } else {
                    Tier::Instance
                };
                if allocation.tier(ctx) != tier {
                    allocation.set_tier(ctx, tier);
                    status = IRStatus::Changed;
                }
            }
            if let Some(array) = Operation::get_op::<crate::collections::ArrayNewOp>(op, ctx)
                && array
                    .get_attr_perry_array_tier(ctx)
                    .is_none_or(|tier| *tier != Tier::Instance)
            {
                array.set_attr_perry_array_tier(ctx, Tier::Instance);
                status = IRStatus::Changed;
            }
        }
    }
    status
}
