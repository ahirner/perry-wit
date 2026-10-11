//! Reuse successful optional unwraps within their dominance scope.
use crate::{aggregates::functions, exceptions::CaughtOp, optional::UnwrapOp};
use pliron::{
    basic_block::BasicBlock,
    builtin::op_interfaces::OneRegionInterface,
    context::{Context, Ptr},
    graph::dominance::compute_dominator_tree,
    linked_list::{ContainsLinkedList, LinkedList},
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use std::collections::HashMap;

type Key = (Value, TypeHandle);
enum Visit {
    Enter(Ptr<BasicBlock>),
    Leave(Vec<Key>),
}

pub(crate) fn reuse(root: Ptr<Operation>, ctx: &mut Context) {
    for function in functions(root, ctx) {
        let region = function.get_region(ctx);
        let tree = compute_dominator_tree(ctx, &region);
        let Some(root) = tree.root() else {
            continue;
        };
        let mut work = vec![Visit::Enter(root)];
        let mut successful: HashMap<Key, Value> = HashMap::new();
        while let Some(visit) = work.pop() {
            let block = match visit {
                Visit::Leave(keys) => {
                    for key in keys {
                        successful.remove(&key);
                    }
                    continue;
                }
                Visit::Enter(block) => block,
            };
            let operations: Vec<_> = block.deref(ctx).iter(ctx).collect();
            let mut introduced = Vec::new();
            for op in operations {
                if Operation::get_op::<UnwrapOp>(op, ctx).is_none()
                    || op
                        .deref(ctx)
                        .get_next()
                        .is_some_and(|next| Operation::get_op::<CaughtOp>(next, ctx).is_some())
                {
                    continue;
                }
                let input = op.deref(ctx).get_operand(0);
                let result = op.deref(ctx).get_result(0);
                let key = (input, result.get_type(ctx));
                if let Some(value) = successful.get(&key) {
                    result.replace_all_uses_with(ctx, value);
                    Operation::erase(op, ctx);
                } else {
                    successful.insert(key, result);
                    introduced.push(key);
                }
            }
            work.push(Visit::Leave(introduced));
            work.extend(tree.children(&block).map(Visit::Enter));
        }
    }
}
