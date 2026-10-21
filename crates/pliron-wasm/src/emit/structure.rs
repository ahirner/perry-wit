//! Ramsey's dominator-tree structuring. Loop headers open a `loop`, merge nodes open an enclosing
//! `block`, conditional branches open an `if`, and branches lower to `br` targeting the matching label.
use super::{
    EmitError, EmitResult,
    function::{Frame, FunctionEmitter, successors},
};
use crate::control::{BrOp, BrTableOp, CondBrOp, ReturnOp, UnreachableOp};
use pliron::{
    basic_block::BasicBlock, builtin::op_interfaces::BranchOpInterface, context::Ptr,
    linked_list::ContainsLinkedList, op::Op, operation::Operation, value::Value,
};
use wasm_encoder::{BlockType, Instruction};

impl FunctionEmitter<'_> {
    /// Emit `block` and the blocks it immediately dominates.
    pub(super) fn do_tree(&mut self, block: Ptr<BasicBlock>) -> EmitResult<()> {
        let mut merges: Vec<_> = self
            .dom
            .children(&block)
            .filter(|child| self.merge_nodes.contains(child) && self.order.contains_key(child))
            .collect();
        // The merge child emitted last (highest reverse postorder) is the outermost block.
        merges.sort_by_key(|child| std::cmp::Reverse(self.rpo(*child)));
        if self.loop_headers.contains(&block) {
            self.code.push(Instruction::Loop(BlockType::Empty));
            self.frames.push(Frame::LoopHeadedBy(block));
            self.node_within(block, &merges)?;
            self.frames.pop();
            self.code.push(Instruction::End);
            Ok(())
        } else {
            self.node_within(block, &merges)
        }
    }

    /// Emit `block` inside one `block` per merge child, ordered outermost first so each merge
    /// child follows the `end` of its enclosing `block`.
    fn node_within(
        &mut self,
        block: Ptr<BasicBlock>,
        merges: &[Ptr<BasicBlock>],
    ) -> EmitResult<()> {
        if let Some((&follower, inner)) = merges.split_first() {
            self.code.push(Instruction::Block(BlockType::Empty));
            self.frames.push(Frame::BlockFollowedBy(follower));
            self.node_within(block, inner)?;
            self.frames.pop();
            self.code.push(Instruction::End);
            return self.do_tree(follower);
        }
        let ctx = self.ctx;
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        let Some((&terminator, body)) = ops.split_last() else {
            return Err(self.malformed("empty block"));
        };
        for &op in body {
            self.statement(op)?;
        }
        if let Some(br) = Operation::get_op::<BrOp>(terminator, ctx) {
            let dest = terminator.deref(ctx).get_successor(0);
            let args: Vec<_> = br.get_operation().deref(ctx).operands().collect();
            self.edge(block, dest, &args)
        } else if let Some(cond_br) = Operation::get_op::<CondBrOp>(terminator, ctx) {
            let condition = terminator.deref(ctx).get_operand(0);
            self.value(condition)?;
            self.code.push(Instruction::If(BlockType::Empty));
            self.frames.push(Frame::IfThenElse);
            for succ in 0..2 {
                if succ == 1 {
                    self.code.push(Instruction::Else);
                }
                let dest = terminator.deref(ctx).get_successor(succ);
                let args = cond_br.successor_operands(ctx, succ);
                self.edge(block, dest, &args)?;
            }
            self.frames.pop();
            self.code.push(Instruction::End);
            Ok(())
        } else if Operation::get_op::<BrTableOp>(terminator, ctx).is_some() {
            let index = terminator.deref(ctx).get_operand(0);
            self.value(index)?;
            let mut labels = successors(ctx, block)
                .into_iter()
                .map(|dest| self.label(block, dest))
                .collect::<EmitResult<Vec<_>>>()?;
            let default = labels
                .pop()
                .ok_or_else(|| self.malformed("br_table without default"))?;
            self.code.push(Instruction::BrTable(labels.into(), default));
            Ok(())
        } else if Operation::get_op::<ReturnOp>(terminator, ctx).is_some() {
            let values: Vec<_> = terminator.deref(ctx).operands().collect();
            self.values(&values)?;
            self.code.push(Instruction::Return);
            Ok(())
        } else if Operation::get_op::<UnreachableOp>(terminator, ctx).is_some() {
            self.code.push(Instruction::Unreachable);
            Ok(())
        } else {
            Err(self.unsupported(terminator))
        }
    }

    /// Move `args` into `dest`'s parameters, then transfer control.
    fn edge(
        &mut self,
        from: Ptr<BasicBlock>,
        dest: Ptr<BasicBlock>,
        args: &[Value],
    ) -> EmitResult<()> {
        let ctx = self.ctx;
        let moves: Vec<(Value, u32)> = dest
            .deref(ctx)
            .arguments()
            .zip(args)
            .filter_map(|(param, arg)| {
                let local = *self.locals.get(&param)?;
                // Coalescing may give the argument the parameter's local already.
                let in_place = arg.defining_op().is_none_or(|def| !self.in_place(def))
                    && self.locals.get(arg) == Some(&local);
                (!in_place).then_some((*arg, local))
            })
            .collect();
        let args: Vec<_> = moves.iter().map(|(arg, _)| *arg).collect();
        self.values(&args)?;
        for (_, local) in moves.iter().rev() {
            self.code.push(Instruction::LocalSet(*local));
        }
        self.do_branch(from, dest)
    }

    /// Branch to `dest`: `br` to its label, nothing when control falls through to it, or its
    /// code inline when `from` is its only forward predecessor.
    fn do_branch(&mut self, from: Ptr<BasicBlock>, dest: Ptr<BasicBlock>) -> EmitResult<()> {
        let target = if self.rpo(dest) <= self.rpo(from) {
            Frame::LoopHeadedBy(dest)
        } else if self.merge_nodes.contains(&dest) {
            Frame::BlockFollowedBy(dest)
        } else {
            return self.do_tree(dest);
        };
        let depth = self
            .depth(target)
            .ok_or_else(|| self.malformed("branch target is not an enclosing label"))?;
        // A branch is the last code of its construct, and an `if` the last code of its
        // own: when only `if`s lie between it and the block it ends, falling off the arms
        // reaches the same `end`.
        let falls_through = matches!(target, Frame::BlockFollowedBy(_))
            && self
                .frames
                .iter()
                .rev()
                .take(depth as usize)
                .all(|frame| *frame == Frame::IfThenElse);
        if !falls_through {
            self.code.push(Instruction::Br(depth));
        }
        Ok(())
    }

    /// The label depth of a branch from `from` to `dest`, which must be an enclosing loop or
    /// block (a `br_table` target).
    fn label(&self, from: Ptr<BasicBlock>, dest: Ptr<BasicBlock>) -> EmitResult<u32> {
        let target = if self.rpo(dest) <= self.rpo(from) {
            Frame::LoopHeadedBy(dest)
        } else {
            Frame::BlockFollowedBy(dest)
        };
        self.depth(target)
            .ok_or_else(|| self.malformed("br_table target is not an enclosing label"))
    }

    /// The label index of the innermost enclosing `target`.
    fn depth(&self, target: Frame) -> Option<u32> {
        self.frames
            .iter()
            .rev()
            .position(|frame| *frame == target)
            .map(|depth| depth as u32)
    }

    /// The error for an op outside the wasm dialect.
    pub(super) fn unsupported(&self, op: Ptr<Operation>) -> EmitError {
        EmitError::UnsupportedOp {
            function: self.name.clone(),
            op: Operation::get_op_dyn(op, self.ctx).get_opid().to_string(),
        }
    }
}
