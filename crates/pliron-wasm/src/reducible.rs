//! Making control flow reducible, so that [crate::emit] can structure it.
//!
//! A cycle with several entry blocks gets a *guard* block that every edge into an entry,
//! from outside or inside the cycle, is redirected through. The guard takes an `i32` label
//! naming the original target followed by the block arguments of every entry, and dispatches
//! on the label. It becomes the cycle's only header. This follows the guard/dispatch
//! construction of Johnson and Unger (as in LLVM's `FixIrreducible`): every run takes the
//! original blocks plus the guard, and since any path in the new graph minus the guards is a
//! path in the old one, definitions still dominate their uses.
//!
//! Cycles are found as strongly connected components, outermost first. After fixing a
//! component, the components nested within it (excluding its header) are processed recursively.
use crate::{
    attrs::NumOpcode,
    control::{BrOp, BrTableOp, CondBrOp},
    ops::NumOp,
    types::{I32_BITS, integer_attr},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{BranchOpInterface, OneRegionInterface, OneResultInterface},
        ops::{ConstantOp, FuncOp},
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    graph::traversals::region::DFSTraversal,
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
    operation::Operation,
    region::Region,
    r#type::Typed,
    utils::table::ISet,
};
use std::collections::HashSet;

/// Make the CFG of `function` reducible. Returns the number of guard blocks inserted.
pub fn make_reducible(ctx: &mut Context, function: &FuncOp) -> usize {
    let entry = function.get_entry_block(ctx);
    let region = function.get_region(ctx);
    let mut guards = 0;
    let reachable: ISet<_> = DFSTraversal::new(ctx, &region).pre_order().collect();
    let unreachable: HashSet<_> = region
        .deref(ctx)
        .iter(ctx)
        .filter(|block| !reachable.contains(block))
        .collect();
    let mut work = vec![reachable];
    while let Some(nodes) = work.pop() {
        let edges: Vec<_> = nodes
            .iter()
            .map(|block| {
                block
                    .deref(ctx)
                    .succs(ctx)
                    .into_iter()
                    .filter_map(|target| nodes.get_index_of(&target))
                    .collect()
            })
            .collect();
        for mut component in pliron_control::graph::components(&edges).into_iter().rev() {
            if !component.is_cyclic {
                continue;
            }
            // Source order keeps guard labels and generated blocks deterministic.
            component.nodes.sort_unstable();
            let mut members: ISet<_> = component
                .nodes
                .into_iter()
                .map(|index| nodes[index])
                .collect();
            let entries: Vec<_> = members
                .iter()
                .copied()
                .filter(|block| {
                    *block == entry
                        || block
                            .preds(ctx)
                            .iter()
                            .any(|pred| !unreachable.contains(pred) && !members.contains(pred))
                })
                .collect();
            let header = if entries.len() > 1 {
                assert!(
                    !entries.contains(&entry),
                    "the function entry block cannot be a loop entry"
                );
                guards += 1;
                insert_guard(ctx, region, &entries)
            } else {
                entries[0]
            };
            members.shift_remove(&header);
            if !members.is_empty() {
                work.push(members);
            }
        }
    }
    guards
}

fn i32_constant(ctx: &mut Context, value: usize) -> ConstantOp {
    let attr = integer_attr(ctx, I32_BITS, value as u64);
    ConstantOp::new(ctx, Box::new(attr))
}

fn insert_guard(
    ctx: &mut Context,
    region: Ptr<Region>,
    entries: &[Ptr<BasicBlock>],
) -> Ptr<BasicBlock> {
    let mut types = vec![IntegerType::get(ctx, 32, Signedness::Signless).into()];
    let ranges: Vec<_> = entries
        .iter()
        .map(|entry| {
            let start = types.len();
            types.extend(entry.deref(ctx).arguments().map(|arg| arg.get_type(ctx)));
            start..types.len()
        })
        .collect();
    let guard = BasicBlock::new(ctx, None, types.clone());
    guard.insert_at_back(region, ctx);

    // Rewrite original edges before adding the dispatcher's own edges to the entries.
    let sources: ISet<_> = entries
        .iter()
        .flat_map(|entry| entry.uses(ctx).into_iter().map(|edge| edge.user_op()))
        .collect();
    for source in sources {
        let successors: Vec<_> = source.deref(ctx).successors().collect();
        for (arm, target) in successors.into_iter().enumerate() {
            let Some(tag) = entries.iter().position(|&entry| entry == target) else {
                continue;
            };
            let (term, arm) = if Operation::is_op::<BrTableOp>(source, ctx) {
                // A table has no operands to carry the tag. Only this edge needs a bridge.
                let bridge = BasicBlock::new(ctx, None, vec![]);
                bridge.insert_at_back(region, ctx);
                Operation::replace_successor(source, ctx, arm, bridge);
                let jump = BrOp::new(ctx, guard, vec![]).get_operation();
                jump.insert_at_back(bridge, ctx);
                (jump, 0)
            } else {
                Operation::replace_successor(source, ctx, arm, guard);
                (source, arm)
            };
            let dynamic = Operation::get_op_dyn(term, ctx);
            let branch = op_cast::<dyn BranchOpInterface>(&*dynamic).expect("verified Wasm branch");
            let supplied = branch.successor_operands(ctx, arm);
            for (slot, &ty) in types.iter().enumerate() {
                let value = if ranges[tag].contains(&slot) {
                    supplied[slot - ranges[tag].start]
                } else {
                    let constant = if slot == 0 {
                        i32_constant(ctx, tag)
                    } else {
                        crate::zero_constant(ctx, ty)
                    };
                    constant.get_operation().insert_before(ctx, term);
                    constant.get_result(ctx)
                };
                if slot < supplied.len() {
                    branch.set_successor_operand(ctx, arm, slot, value);
                } else {
                    branch.add_successor_operand(ctx, arm, value);
                }
            }
        }
    }

    let label = guard.deref(ctx).get_argument(0);
    let mut block = guard;
    for (tag, (&entry, slots)) in entries.iter().zip(ranges).enumerate() {
        let args = slots
            .map(|slot| guard.deref(ctx).get_argument(slot))
            .collect();
        if tag + 1 == entries.len() {
            BrOp::new(ctx, entry, args)
                .get_operation()
                .insert_at_back(block, ctx);
            break;
        }
        let matches = if tag == 0 {
            NumOp::new(ctx, NumOpcode::I32Eqz, vec![label])
        } else {
            let tag = i32_constant(ctx, tag);
            tag.get_operation().insert_at_back(block, ctx);
            NumOp::new(ctx, NumOpcode::I32Eq, vec![label, tag.get_result(ctx)])
        };
        matches.get_operation().insert_at_back(block, ctx);
        let next = BasicBlock::new(ctx, None, vec![]);
        next.insert_at_back(region, ctx);
        CondBrOp::new(ctx, matches.get_result(ctx), entry, args, next, vec![])
            .get_operation()
            .insert_at_back(block, ctx);
        block = next;
    }
    guard
}
