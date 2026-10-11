//! Expose allocation-free append paths before liveness and collection planning.
use crate::{
    aggregates::{functions, operations},
    collections::StringAppendOp,
    control::{BrOp, CondBrOp},
    intrinsics::{Intrinsic, IntrinsicOp},
};
use pliron::{
    basic_block::BasicBlock,
    context::{Context, Ptr},
    irbuild::{
        inserter::OpInsertionPoint,
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    op::Op,
    operation::Operation,
    r#type::Typed,
};

/// Ownership analysis already established exclusive headers. A successful try-append
/// mutates that header without allocating; failure leaves it unchanged for the ordinary
/// append. Only the fallback can collect, so its own live operands determine its roots.
pub(crate) fn expand(root: Ptr<Operation>, ctx: &mut Context) {
    for function in functions(root, ctx) {
        for op in operations(ctx, &function) {
            let kind = if Operation::get_op::<StringAppendOp>(op, ctx).is_some() {
                Intrinsic::StringTryAppend
            } else if Operation::get_op::<IntrinsicOp>(op, ctx).and_then(|i| i.intrinsic(ctx))
                == Some(Intrinsic::StringScalarAppend)
            {
                Intrinsic::StringScalarTryAppend
            } else {
                continue;
            };
            let result = op.deref(ctx).get_result(0);
            let operands: Vec<_> = op.deref(ctx).operands().collect();
            let prefix = operands[0];
            let block = op.deref(ctx).get_parent_block().expect("append in block");
            let mut rewriter = IRRewriter::<DummyListener>::default();
            let continuation =
                rewriter.split_block(ctx, block, OpInsertionPoint::AfterOperation(op), None);
            BasicBlock::insert_argument(continuation, ctx, 0, result.get_type(ctx));
            let joined = continuation.deref(ctx).get_argument(0);
            result.replace_all_uses_with(ctx, &joined);
            let fallback =
                rewriter.split_block(ctx, block, OpInsertionPoint::BeforeOperation(op), None);
            let attempt = IntrinsicOp::new(ctx, kind, operands).get_operation();
            attempt.insert_at_back(block, ctx);
            let condition = attempt.deref(ctx).get_result(0);
            CondBrOp::new(ctx, condition, continuation, vec![prefix], fallback, vec![])
                .get_operation()
                .insert_at_back(block, ctx);
            BrOp::new(ctx, continuation, vec![result])
                .get_operation()
                .insert_at_back(fallback, ctx);
        }
    }
}
