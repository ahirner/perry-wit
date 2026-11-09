//! Reuse successful optional unwraps within their dominance scope.
use crate::{aggregates::functions, exceptions::CaughtOp, optional::UnwrapOp};
use pliron::{
    builtin::op_interfaces::OneRegionInterface,
    context::{Context, Ptr},
    graph::{
        dominance::DomInfo,
        walkers::{Direction, interruptible::walk_advance},
    },
    irbuild::{
        IRStatus,
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    linked_list::{ContainsLinkedList, LinkedList},
    operation::Operation,
    pass::{AnalysisManager, PassResult},
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use std::collections::HashMap;

type Key = (Value, TypeHandle);

pub(crate) fn reuse_unwraps(
    root: Ptr<Operation>,
    ctx: &mut Context,
    analyses: &mut AnalysisManager,
) -> Result<PassResult> {
    analyses.get_analysis::<DomInfo>(root, ctx)?;
    let mut dominance = analyses
        .try_get_analysis_mut::<DomInfo>(root)
        .expect("dominance");
    let mut rewriter = IRRewriter::<DummyListener>::default();
    let mut result = PassResult::default();
    for function in functions(root, ctx) {
        let region = function.get_region(ctx);
        let tree = dominance.get_dom_tree(ctx, region);
        let mut state = (HashMap::<Key, Value>::new(), Vec::<Vec<Key>>::new());
        let _ = tree.walk(
            &mut state,
            Direction::Forward,
            |(successful, scopes), block| {
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
                        rewriter.replace_value_uses_with(ctx, result, *value);
                        rewriter.erase_operation(ctx, op);
                    } else {
                        successful.insert(key, result);
                        introduced.push(key);
                    }
                }
                scopes.push(introduced);
                walk_advance::<()>()
            },
            |(successful, scopes), _| {
                for key in scopes.pop().expect("entered dominance scope") {
                    successful.remove(&key);
                }
                walk_advance()
            },
        );
    }
    result.ir_changed = if rewriter.is_modified() {
        IRStatus::Changed
    } else {
        IRStatus::Unchanged
    };
    result.set_preserved::<DomInfo>();
    Ok(result)
}
