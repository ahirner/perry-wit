//! Expose allocation-free append paths before liveness and collection planning.
use crate::{
    collections::ConcatReuseOp,
    control::{BrOp, CondBrOp},
    intrinsics::{Intrinsic, IntrinsicOp},
};
use pliron::{
    basic_block::BasicBlock,
    context::{Context, Ptr},
    irbuild::{
        inserter::{Inserter, OpInsertionPoint},
        match_rewrite::{MatchRewrite, MatchRewriter},
        rewriter::Rewriter,
    },
    op::Op,
    operation::Operation,
    result::Result,
    r#type::Typed,
};
use std::collections::HashSet;

/// Ownership analysis establishes exclusive sequence headers before this pass runs. A
/// successful try-append mutates that header in place, and a full header falls back to an
/// ordinary append. Only the fallback path can trigger collection, so its live operands
/// determine its roots.
#[derive(Default)]
pub(crate) struct ExpandAppends {
    expanded: HashSet<Ptr<Operation>>,
}

impl MatchRewrite for ExpandAppends {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        !self.expanded.contains(&op)
            && ((Operation::get_op::<ConcatReuseOp>(op, ctx).is_some()
                && op
                    .deref(ctx)
                    .get_result(0)
                    .get_type(ctx)
                    .deref(ctx)
                    .is::<crate::types::StringType>())
                || crate::intrinsics::intrinsic_of(ctx, op) == Some(Intrinsic::StringScalarAppend))
    }
    fn rewrite(
        &mut self,
        ctx: &mut Context,
        rewriter: &mut MatchRewriter,
        op: Ptr<Operation>,
    ) -> Result<()> {
        if !self.expanded.insert(op) {
            return Ok(());
        }
        let kind = if Operation::get_op::<ConcatReuseOp>(op, ctx).is_some() {
            Intrinsic::StringTryAppend
        } else {
            Intrinsic::StringScalarTryAppend
        };
        let result = op.deref(ctx).get_result(0);
        let operands: Vec<_> = op.deref(ctx).operands().collect();
        let prefix = operands[0];
        let block = op.deref(ctx).get_parent_block().expect("append in block");
        let continuation =
            rewriter.split_block(ctx, block, OpInsertionPoint::AfterOperation(op), None);
        BasicBlock::insert_argument(continuation, ctx, 0, result.get_type(ctx));
        let joined = continuation.deref(ctx).get_argument(0);
        rewriter.replace_value_uses_with(ctx, result, joined);
        let fallback =
            rewriter.split_block(ctx, block, OpInsertionPoint::BeforeOperation(op), None);
        let attempt = IntrinsicOp::new(ctx, kind, operands).get_operation();
        rewriter.set_insertion_point_to_block_end(block);
        rewriter.insert_operation(ctx, attempt);
        let condition = attempt.deref(ctx).get_result(0);
        let branch = CondBrOp::new(ctx, condition, continuation, vec![prefix], fallback, vec![]);
        rewriter.insert_op(ctx, &branch);
        rewriter.set_insertion_point_to_block_end(fallback);
        let branch = BrOp::new(ctx, continuation, vec![result]);
        rewriter.insert_op(ctx, &branch);
        Ok(())
    }
}
