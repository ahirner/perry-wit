use pliron::{
    context::{Context, Ptr},
    linked_list::{ContainsLinkedList, LinkedList},
    operation::Operation,
};
use std::{collections::HashSet, hash::Hash};

/// Walk every path backwards from `consumer` to its last `origin` execution.
/// A transition returning `None` invalidates the path, and control-flow joins retain each distinct state.
pub(crate) fn preserves<S: Copy + Eq + Hash>(
    ctx: &Context,
    origin: Ptr<Operation>,
    consumer: Ptr<Operation>,
    state: S,
    mut transition: impl FnMut(Ptr<Operation>, S) -> Option<S>,
) -> bool {
    if origin == consumer {
        return true;
    }
    let Some(block) = consumer.deref(ctx).get_parent_block() else {
        return false;
    };
    let mut pending = vec![(block, Some(consumer), state)];
    let mut seen = HashSet::new();
    let mut reached_origin = false;
    while let Some((block, before, mut state)) = pending.pop() {
        if before.is_none() && !seen.insert((block, state)) {
            continue;
        }
        let mut cursor = before.map_or_else(
            || block.deref(ctx).get_tail(),
            |op| op.deref(ctx).get_prev(),
        );
        let mut refreshed = false;
        while let Some(op) = cursor {
            if op == origin {
                refreshed = true;
                reached_origin = true;
                break;
            }
            let Some(next) = transition(op, state) else {
                return false;
            };
            state = next;
            cursor = op.deref(ctx).get_prev();
        }
        if !refreshed {
            let predecessors = block.preds(ctx);
            if predecessors.is_empty() {
                return false;
            }
            pending.extend(predecessors.into_iter().map(|block| (block, None, state)));
        }
    }
    reached_origin
}
