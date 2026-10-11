//! Bounded frame storage for closed scalar records with no escaping aliases.
use crate::{
    attrs::Tier,
    effects,
    heap::{AllocOp, FieldGetOp, FieldSetOp},
};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, OneResultInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    operation::Operation,
    r#type::{Typed, TypedHandle},
};

/// A fixed small frame permits iteration reuse only when the allocation's SSA value never
/// flows through a join, a store or a retaining callee. Reference fields remain collectible
/// until their longer-lived children and mutation effects have stronger proofs.
fn frame_candidates(
    ctx: &Context,
    function: &FuncOp,
    summaries: &std::collections::HashMap<pliron::identifier::Identifier, effects::Summary>,
) -> Vec<AllocOp> {
    if crate::calls::is_async(ctx, function) {
        return vec![];
    }
    let mut bytes = 0;
    function
        .get_region(ctx)
        .deref(ctx)
        .iter(ctx)
        .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
        .filter_map(|op| Operation::get_op::<AllocOp>(op, ctx))
        .filter(|allocation| {
            let value = allocation.get_result(ctx);
            let record =
                TypedHandle::<crate::types::RecordType>::from_handle(value.get_type(ctx), ctx)
                    .expect("verified record");
            let record = record.deref(ctx);
            let fields = record.fields().expect("verified fields");
            let size = 8 + 8 * fields.len();
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
                    && (Operation::get_op::<FieldGetOp>(op, ctx).is_some()
                        || Operation::get_op::<FieldSetOp>(op, ctx).is_some())
                {
                    return true;
                }
                Operation::get_op::<crate::calls::CallOp>(op, ctx)
                    .and_then(|call| summaries.get(&call.callee(ctx)))
                    .is_some_and(|summary| !summary.escapes.contains(&operand))
            });
            if local {
                bytes += size;
            }
            local
        })
        .collect()
}

pub(crate) fn assign(root: Ptr<Operation>, ctx: &Context) {
    let functions = crate::aggregates::functions(root, ctx);
    let summaries = effects::summarize(ctx, &functions);
    for function in &functions {
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for op in block.deref(ctx).iter(ctx) {
                if let Some(allocation) = Operation::get_op::<AllocOp>(op, ctx) {
                    allocation.set_tier(ctx, Tier::Instance);
                }
                if let Some(array) = Operation::get_op::<crate::collections::ArrayNewOp>(op, ctx) {
                    array.set_attr_perry_array_tier(ctx, Tier::Instance);
                }
            }
        }
        for allocation in frame_candidates(ctx, function, &summaries) {
            allocation.set_tier(ctx, Tier::Frame);
        }
    }
}
