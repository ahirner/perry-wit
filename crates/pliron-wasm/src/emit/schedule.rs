//! Stack scheduling: single-use values evaluated in place at their user (tree folding).
use super::function::FunctionEmitter;
use crate::{
    attrs::Effect,
    control::{BrOp, CondBrOp},
    ops::{NumOp, SelectOp},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::ops::ConstantOp,
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    operation::Operation,
    value::Value,
};
use std::collections::HashMap;

/// Constants are rematerialized at every use: `i32.const 0` is as short as a `local.get`.
pub(super) fn is_constant(ctx: &Context, op: Ptr<Operation>) -> bool {
    Operation::get_op::<ConstantOp>(op, ctx).is_some()
}

/// Whether evaluating `op` later than its position is unobservable: no effects, no traps,
/// no memory reads.
pub(super) fn movable(ctx: &Context, op: Ptr<Operation>) -> bool {
    is_constant(ctx, op)
        || Operation::get_op::<SelectOp>(op, ctx).is_some()
        || Operation::get_op::<NumOp>(op, ctx)
            .is_some_and(|num| num.opcode(ctx).effect() == Effect::Pure)
}

impl FunctionEmitter<'_> {
    /// Decide which single-use values are evaluated in place at their user.
    pub(super) fn schedule(&mut self, block: Ptr<BasicBlock>) {
        let ctx = self.ctx;
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        let index: HashMap<_, _> = ops.iter().enumerate().map(|(i, op)| (*op, i)).collect();
        // Whether an op's evaluation tree has an effect, and the earliest op in that tree.
        let mut anchored: Vec<bool> = ops.iter().map(|op| !movable(ctx, *op)).collect();
        let mut start: Vec<usize> = (0..ops.len()).collect();
        for (user_index, &user) in ops.iter().enumerate() {
            // Branch arguments are evaluated on the edge, after the condition and only on the
            // taken edge, so only effect-free trees fold into them.
            let is_cond_br = Operation::get_op::<CondBrOp>(user, ctx).is_some();
            let is_branch = is_cond_br || Operation::get_op::<BrOp>(user, ctx).is_some();
            let operands: Vec<Value> = user.deref(ctx).operands().collect();
            let mut barrier = user_index;
            let mut cursor = operands.len();
            while cursor > 0 {
                let position = cursor - 1;
                let operand = operands[position];
                let edge_argument = is_branch && !(position == 0 && is_cond_br);
                let Some(def) = operand.defining_op() else {
                    cursor -= 1;
                    continue;
                };
                let Some(&def_index) = index.get(&def) else {
                    cursor -= 1;
                    continue;
                };
                let results: Vec<Value> = def.deref(ctx).results().collect();
                let arity = results.len();
                if arity == 0
                    || arity > cursor
                    || operands[cursor - arity..cursor] != results[..]
                    || def_index >= user_index
                    || results.iter().any(|res| res.num_uses(ctx) != 1)
                    || self.folded.contains(&def)
                    || is_constant(ctx, def)
                    || (edge_argument && anchored[def_index])
                {
                    cursor -= 1;
                    continue;
                }
                if anchored[def_index] {
                    // An effectful tree may only move past pure statements.
                    let crosses_effect = (def_index + 1..barrier)
                        .any(|between| anchored[between] && !self.folded.contains(&ops[between]));
                    if crosses_effect {
                        cursor -= 1;
                        continue;
                    }
                    barrier = start[def_index];
                }
                self.folded.insert(def);
                anchored[user_index] |= anchored[def_index];
                start[user_index] = start[user_index].min(start[def_index]);
                cursor -= arity;
            }
        }
    }

    /// Whether `op` is evaluated in place at its user on the operand stack.
    pub(super) fn in_place(&self, op: Ptr<Operation>) -> bool {
        self.folded.contains(&op) || is_constant(self.ctx, op)
    }
}
