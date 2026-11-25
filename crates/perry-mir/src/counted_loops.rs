//! Fold a side-effect-free SSA loop whose counters start at positive zero and advance together.
//! With a strict upper bound converted from a 32-bit integer, unit increments in f64 are exact.
//! Each counter exits at max(0, bound), provided its representation can hold the trip count.
use crate::{
    ArrayLenOp, BinaryKind, BinaryOp, BrOp, CompareKind, CompareOp, CondBrOp, Intrinsic,
    IntrinsicOp, NumericCastOp, NumericConstant, StringLenOp,
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{BranchOpInterface, OneResultInterface},
        ops::ConstantOp,
        types::{FP64Type, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    irbuild::{
        inserter::Inserter,
        match_rewrite::{MatchRewrite, MatchRewriter},
        rewriter::Rewriter,
    },
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    opts::dce::has_side_effects,
    result::Result,
    r#type::Typed,
    value::Value,
};

fn literal(ctx: &Context, value: Value) -> Option<u64> {
    let op = Operation::get_op::<ConstantOp>(value.defining_op()?, ctx)?;
    let attr = op.get_attr_builtin_constant_value(ctx)?;
    match NumericConstant::from_scalar(ctx, &**attr)? {
        NumericConstant::Signed(n) => n.try_into().ok(),
        NumericConstant::Unsigned(n) => Some(n),
        NumericConstant::Float(_) => None,
    }
}

struct CountedLoop {
    branch: CondBrOp,
    header: Ptr<BasicBlock>,
    comparison: Ptr<Operation>,
    bound: Value,
    minimum: Option<Value>,
}

impl CountedLoop {
    fn recognize(ctx: &Context, op: Ptr<Operation>) -> Option<Self> {
        let branch = Operation::get_op::<CondBrOp>(op, ctx)?;
        let header = op.deref(ctx).get_parent_block()?;
        let body = op.deref(ctx).get_successor(0);
        let exit = op.deref(ctx).get_successor(1);
        let latch = body.deref(ctx).get_terminator(ctx)?;
        if header
            .deref(ctx)
            .get_parent_region()?
            .deref(ctx)
            .get_entry_block()
            == Some(header)
            || body == header
            || exit == header
            || exit == body
            || body.uses(ctx).len() != 1
            || !Operation::is_op::<BrOp>(latch, ctx)
            || latch.deref(ctx).get_successor(0) != header
        {
            return None;
        }
        let incoming = header.uses(ctx);
        let [a, b] = incoming.as_slice() else {
            return None;
        };
        let entry = if a.user_op() == latch {
            b.user_op()
        } else {
            a.user_op()
        };
        if !Operation::is_op::<BrOp>(entry, ctx) {
            return None;
        }

        let comparison = branch.get_operand_condition(ctx).defining_op()?;
        let test = Operation::get_op::<CompareOp>(comparison, ctx)?;
        let index = comparison.deref(ctx).get_operand(0);
        let bound = comparison.deref(ctx).get_operand(1);
        let index_pos = header.deref(ctx).arguments().position(|arg| arg == index)?;
        if test.kind(ctx) != CompareKind::Lt
            || comparison.deref(ctx).get_parent_block() != Some(header)
            || !bound.get_type(ctx).deref(ctx).is::<FP64Type>()
        {
            return None;
        }
        let cast = bound.defining_op()?;
        Operation::get_op::<NumericCastOp>(cast, ctx)?;
        let input = cast.deref(ctx).get_operand(0);
        let input_ty = input.get_type(ctx).deref(ctx);
        let integer = input_ty.downcast_ref::<IntegerType>()?;
        if integer.width() > 32 {
            return None;
        }
        let unsigned = integer.signedness() == Signedness::Unsigned;
        let length = input
            .defining_op()
            .filter(|&def| crate::is_any::<(ArrayLenOp, StringLenOp)>(ctx, def));
        if input.get_parent_block(ctx) == Some(header) && length.is_none() {
            return None;
        }
        if header
            .deref(ctx)
            .iter(ctx)
            .any(|item| item != op && item != comparison && item != cast && Some(item) != length)
        {
            return None;
        }
        for (i, arg) in header.deref(ctx).arguments().enumerate() {
            let ty = arg.get_type(ctx).deref(ctx);
            if !ty.is::<FP64Type>()
                && !ty.downcast_ref::<IntegerType>().is_some_and(|ty| {
                    ty.width() == 64
                        || (ty.width() == 32
                            && (!unsigned || ty.signedness() == Signedness::Unsigned))
                })
            {
                return None;
            }
            let next = latch.deref(ctx).get_operand(i).defining_op()?;
            let add = Operation::get_op::<BinaryOp>(next, ctx)?;
            if add.kind(ctx) != BinaryKind::Add
                || next.deref(ctx).get_parent_block() != Some(body)
                || next.deref(ctx).get_operand(0) != arg
                || literal(ctx, next.deref(ctx).get_operand(1)) != Some(1)
                || literal(ctx, entry.deref(ctx).get_operand(i)) != Some(0)
            {
                return None;
            }
        }
        if body
            .deref(ctx)
            .iter(ctx)
            .any(|item| item != latch && has_side_effects(item, ctx))
        {
            return None;
        }
        Some(Self {
            branch,
            header,
            comparison,
            bound,
            minimum: (!unsigned).then(|| entry.deref(ctx).get_operand(index_pos)),
        })
    }
}

pub(super) struct Fold;

impl MatchRewrite for Fold {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        Operation::is_op::<CondBrOp>(op, ctx)
    }

    fn rewrite(
        &mut self,
        ctx: &mut Context,
        rewriter: &mut MatchRewriter,
        op: Ptr<Operation>,
    ) -> Result<()> {
        let Some(count) = CountedLoop::recognize(ctx, op) else {
            return Ok(());
        };
        rewriter.set_insertion_point_before_operation(count.comparison);
        let trips = if let Some(zero) = count.minimum {
            let max = IntrinsicOp::new(ctx, Intrinsic::MathMax, vec![zero, count.bound]);
            rewriter.insert_op(ctx, &max);
            max.get_operation().deref(ctx).get_result(0)
        } else {
            count.bound
        };
        let counters: Vec<_> = count.header.deref(ctx).arguments().collect();
        for arg in counters {
            let ty = arg.get_type(ctx);
            let value = if ty == trips.get_type(ctx) {
                trips
            } else {
                let cast = NumericCastOp::new(ctx, trips, ty);
                rewriter.insert_op(ctx, &cast);
                cast.get_result(ctx)
            };
            rewriter.replace_value_uses_with(ctx, arg, value);
        }
        let body = op.deref(ctx).get_successor(0);
        let exit = op.deref(ctx).get_successor(1);
        let branch = BrOp::new(ctx, exit, count.branch.successor_operands(ctx, 1));
        rewriter.set_insertion_point_before_operation(op);
        rewriter.insert_op(ctx, &branch);
        rewriter.replace_operation(ctx, op, branch.get_operation());
        rewriter.erase_block(ctx, body);
        Ok(())
    }
}
