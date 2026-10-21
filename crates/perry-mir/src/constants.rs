//! Scalar semantics for pliron's sparse conditional constant propagation.
use crate::{
    attrs::{BinaryKind, CompareKind},
    control::{BrOp, CondBrOp},
    scalar::{BinaryOp, CompareOp, NumericCastOp},
};
use pliron::{
    attribute::AttrObj,
    basic_block::BasicBlock,
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
        op_interfaces::{BranchOpInterface, OneRegionInterface, OneResultInterface},
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    derive::op_interface_impl,
    irbuild::{IRStatus, rewriter::Rewriter},
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
    operation::Operation,
    opts::constants::{BranchOpFoldInterface, ConstFoldInterface},
    r#type::{Typed, TypedHandle},
    utils::apint::{APInt, bw},
};
use std::cmp::Ordering::{Equal, Greater, Less};

/// Remove unused basic-block arguments while preserving entry-block function parameters.
pub(crate) fn remove_unused_block_arguments(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    let mut status = IRStatus::Unchanged;
    for function in crate::aggregates::functions(root, ctx) {
        let blocks: Vec<_> = function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
            .skip(1)
            .collect();
        for block in blocks {
            let arguments: Vec<_> = block.deref(ctx).arguments().collect();
            for (index, argument) in arguments.into_iter().enumerate().rev() {
                if argument.is_used(ctx) {
                    continue;
                }
                for predecessor in block.uses(ctx) {
                    let successor = predecessor.find_index(ctx);
                    let branch = Operation::get_op_dyn(predecessor.user_op(), ctx);
                    op_cast::<dyn BranchOpInterface>(&*branch)
                        .expect("MIR branch")
                        .remove_successor_operand(ctx, successor, index);
                }
                BasicBlock::remove_argument(block, ctx, index);
                status = IRStatus::Changed;
            }
        }
    }
    status
}

#[op_interface_impl]
impl ConstFoldInterface for BinaryOp {
    fn check_fold(&self, ctx: &Context, operands: &[Option<AttrObj>]) -> Vec<Option<AttrObj>> {
        let folded = (|| {
            let (lhs, rhs) = (operands[0].as_ref()?, operands[1].as_ref()?);
            if let (Some(lhs), Some(rhs)) = (
                lhs.downcast_ref::<FPDoubleAttr>(),
                rhs.downcast_ref::<FPDoubleAttr>(),
            ) {
                let (lhs, rhs) = (f64::from(lhs.clone()), f64::from(rhs.clone()));
                let result = match self.kind(ctx) {
                    BinaryKind::Add => lhs + rhs,
                    BinaryKind::Sub => lhs - rhs,
                    BinaryKind::Mul => lhs * rhs,
                    BinaryKind::Div => lhs / rhs,
                    BinaryKind::Rem => lhs % rhs,
                };
                return Some(Box::new(FPDoubleAttr::from(result)) as AttrObj);
            }
            let (lhs, rhs) = (
                lhs.downcast_ref::<IntegerAttr>()?,
                rhs.downcast_ref::<IntegerAttr>()?,
            );
            let result = match self.kind(ctx) {
                BinaryKind::Add => lhs.value().add(&rhs.value()),
                BinaryKind::Sub => lhs.value().sub(&rhs.value()),
                BinaryKind::Mul => lhs.value().mul(&rhs.value()),
                BinaryKind::Div | BinaryKind::Rem => return None,
            };
            Some(Box::new(IntegerAttr::new(lhs.get_type(), result)))
        })();
        vec![folded]
    }

    fn fold_in_place(
        &self,
        ctx: &mut Context,
        operands: &[Option<AttrObj>],
        rewriter: &mut dyn Rewriter,
    ) -> IRStatus {
        self.fold_with_materialization(ctx, operands, rewriter)
    }
}

#[op_interface_impl]
impl ConstFoldInterface for NumericCastOp {
    fn check_fold(&self, ctx: &Context, operands: &[Option<AttrObj>]) -> Vec<Option<AttrObj>> {
        let folded = (|| {
            let input = operands[0].as_ref()?;
            let target = self.get_result(ctx).get_type(ctx);
            if target.deref(ctx).is::<crate::types::BigIntType>() {
                return None;
            }
            let scalar = input
                .downcast_ref::<crate::attrs::NumericConstant>()
                .map(|number| number.scalar(ctx));
            let input = scalar.as_deref().unwrap_or(input.as_ref());
            if target.deref(ctx).is::<crate::types::NumericType>() {
                return crate::attrs::NumericConstant::from_scalar(ctx, input)
                    .map(|number| Box::new(number) as AttrObj);
            }
            let target_int = TypedHandle::<IntegerType>::from_handle(target, ctx).ok();
            if let Some(input) = input.downcast_ref::<IntegerAttr>() {
                let source = input.get_type().deref(ctx);
                let signed = source.signedness() != Signedness::Unsigned;
                if let Some(target) = target_int {
                    let bits = target.deref(ctx).width();
                    let width = bw(bits as usize);
                    let value = if bits < source.width() {
                        input.value().trunc(width)
                    } else if signed {
                        input.value().sext(width)
                    } else {
                        input.value().zext(width)
                    };
                    return Some(Box::new(IntegerAttr::new(target, value)) as AttrObj);
                }
                let value = if signed {
                    input.value().to_i64() as f64
                } else {
                    input.value().to_u64() as f64
                };
                return Some(Box::new(FPDoubleAttr::from(value)) as AttrObj);
            }
            let value = f64::from(input.downcast_ref::<FPDoubleAttr>()?.clone());
            let Some(target) = target_int else {
                return Some(Box::new(FPDoubleAttr::from(value)) as AttrObj);
            };
            let integer = target.deref(ctx);
            let bits = match (
                integer.width() == 64,
                integer.signedness() != Signedness::Unsigned,
            ) {
                (false, true) => value as i32 as u64,
                (false, false) => value as u32 as u64,
                (true, true) => value as i64 as u64,
                (true, false) => value as u64,
            };
            Some(Box::new(IntegerAttr::new(
                target,
                APInt::from_u64(bits, bw(integer.width() as usize)),
            )) as AttrObj)
        })();
        vec![folded]
    }

    fn fold_in_place(
        &self,
        ctx: &mut Context,
        operands: &[Option<AttrObj>],
        rewriter: &mut dyn Rewriter,
    ) -> IRStatus {
        self.fold_with_materialization(ctx, operands, rewriter)
    }
}

#[op_interface_impl]
impl ConstFoldInterface for CompareOp {
    fn check_fold(&self, ctx: &Context, operands: &[Option<AttrObj>]) -> Vec<Option<AttrObj>> {
        if operands.iter().any(Option::is_none) {
            let interval = |index: usize| {
                let value = self.get_operation().deref(ctx).get_operand(index);
                let ty = value.get_type(ctx).deref(ctx);
                let ty = ty.downcast_ref::<IntegerType>()?;
                let bits = ty.width();
                if bits > 64 {
                    return None;
                }
                let unsigned = ty.signedness() == Signedness::Unsigned || bits == 1;
                if let Some(attr) = operands[index].as_ref() {
                    let integer = attr.downcast_ref::<IntegerAttr>()?;
                    let value = if unsigned {
                        integer.value().to_u64() as i128
                    } else {
                        integer.value().to_i64() as i128
                    };
                    Some((value, value))
                } else if unsigned {
                    Some((0, (1i128 << bits) - 1))
                } else {
                    Some((-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1))
                }
            };
            if let (Some((a_min, a_max)), Some((b_min, b_max))) = (interval(0), interval(1)) {
                let result = match self.kind(ctx) {
                    CompareKind::Lt if a_max < b_min => Some(true),
                    CompareKind::Lt if a_min >= b_max => Some(false),
                    CompareKind::Le if a_max <= b_min => Some(true),
                    CompareKind::Le if a_min > b_max => Some(false),
                    CompareKind::Gt if a_min > b_max => Some(true),
                    CompareKind::Gt if a_max <= b_min => Some(false),
                    CompareKind::Ge if a_min >= b_max => Some(true),
                    CompareKind::Ge if a_max < b_min => Some(false),
                    CompareKind::Eq | CompareKind::Ne if a_max < b_min || b_max < a_min => {
                        Some(self.kind(ctx) == CompareKind::Ne)
                    }
                    _ => None,
                };
                if let Some(result) = result {
                    return vec![Some(boolean(ctx, result))];
                }
            }
        }
        let folded = (|| {
            let (lhs, rhs) = (operands[0].as_ref()?, operands[1].as_ref()?);
            let ordering = if let (Some(lhs), Some(rhs)) = (
                lhs.downcast_ref::<FPDoubleAttr>(),
                rhs.downcast_ref::<FPDoubleAttr>(),
            ) {
                f64::from(lhs.clone()).partial_cmp(&f64::from(rhs.clone()))
            } else if let (Some(lhs), Some(rhs)) = (
                lhs.downcast_ref::<crate::attrs::NumericConstant>(),
                rhs.downcast_ref::<crate::attrs::NumericConstant>(),
            ) {
                lhs.order(*rhs)
            } else {
                let (lhs, rhs) = (
                    lhs.downcast_ref::<IntegerAttr>()?,
                    rhs.downcast_ref::<IntegerAttr>()?,
                );
                let ty = lhs.get_type().deref(ctx);
                let less = if ty.signedness() == Signedness::Unsigned || ty.width() == 1 {
                    lhs.value().ult(&rhs.value())
                } else {
                    lhs.value().slt(&rhs.value())
                };
                Some(if lhs.value() == rhs.value() {
                    Equal
                } else if less {
                    Less
                } else {
                    Greater
                })
            };
            let result = match self.kind(ctx) {
                CompareKind::Lt => ordering == Some(Less),
                CompareKind::Le => matches!(ordering, Some(Less | Equal)),
                CompareKind::Gt => ordering == Some(Greater),
                CompareKind::Ge => matches!(ordering, Some(Greater | Equal)),
                CompareKind::Eq => ordering == Some(Equal),
                CompareKind::Ne => ordering != Some(Equal),
            };
            Some(boolean(ctx, result))
        })();
        vec![folded]
    }

    fn fold_in_place(
        &self,
        ctx: &mut Context,
        operands: &[Option<AttrObj>],
        rewriter: &mut dyn Rewriter,
    ) -> IRStatus {
        self.fold_with_materialization(ctx, operands, rewriter)
    }
}

/// The folded `i1` of a comparison.
fn boolean(ctx: &Context, value: bool) -> AttrObj {
    let ty = IntegerType::get(ctx, 1, Signedness::Signless);
    Box::new(IntegerAttr::new(
        ty,
        APInt::from_u64(u64::from(value), bw(1)),
    ))
}

#[op_interface_impl]
impl BranchOpFoldInterface for BrOp {
    fn check_fold(&self, ctx: &Context, _: &[Option<AttrObj>]) -> Vec<Ptr<BasicBlock>> {
        self.get_operation().deref(ctx).successors().collect()
    }

    fn fold_in_place(
        &self,
        _: &mut Context,
        _: &[Option<AttrObj>],
        _: &mut dyn Rewriter,
    ) -> IRStatus {
        IRStatus::Unchanged
    }
}

#[op_interface_impl]
impl BranchOpFoldInterface for CondBrOp {
    fn check_fold(&self, ctx: &Context, operands: &[Option<AttrObj>]) -> Vec<Ptr<BasicBlock>> {
        match operands[0]
            .as_ref()
            .and_then(|attr| attr.downcast_ref::<IntegerAttr>())
        {
            Some(condition) => vec![
                self.get_operation()
                    .deref(ctx)
                    .get_successor(usize::from(condition.value().is_zero())),
            ],
            None => self.get_operation().deref(ctx).successors().collect(),
        }
    }

    fn fold_in_place(
        &self,
        ctx: &mut Context,
        operands: &[Option<AttrObj>],
        rewriter: &mut dyn Rewriter,
    ) -> IRStatus {
        let Some(condition) = operands[0]
            .as_ref()
            .and_then(|attr| attr.downcast_ref::<IntegerAttr>())
        else {
            return IRStatus::Unchanged;
        };
        let successor = usize::from(condition.value().is_zero());
        let destination = self.get_operation().deref(ctx).get_successor(successor);
        let branch =
            BrOp::new(ctx, destination, self.successor_operands(ctx, successor)).get_operation();
        rewriter.insert_operation(ctx, branch);
        rewriter.replace_operation(ctx, self.get_operation(), branch);
        IRStatus::Changed
    }
}
