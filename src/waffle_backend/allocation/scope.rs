//! Canonical scratch buffers retained until their owning adapter finishes.

use super::AllocationFuncs;
use waffle::{
    Block, BlockTarget, FunctionBody, Memory, MemoryArg, Operator, Terminator, Type, Value,
};

pub(crate) struct ScratchScope {
    anchor: Value,
    memory: Memory,
    allocator: AllocationFuncs,
}

impl ScratchScope {
    pub(crate) fn new(
        body: &mut FunctionBody,
        block: Block,
        memory: Memory,
        allocator: AllocationFuncs,
    ) -> Self {
        let one = body.add_op(block, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let anchor = body.add_op(
            block,
            Operator::Call {
                function_index: allocator.frame_new,
            },
            &[one],
            &[Type::I32],
        );
        Self {
            anchor,
            memory,
            allocator,
        }
    }

    /// Each allocation gets a root, including repeated allocations in a canonical list loop.
    pub(crate) fn retain(&self, body: &mut FunctionBody, block: Block, pointer: Value) {
        let two = body.add_op(block, Operator::I32Const { value: 2 }, &[], &[Type::I32]);
        let frame = body.add_op(
            block,
            Operator::Call {
                function_index: self.allocator.frame_new,
            },
            &[two],
            &[Type::I32],
        );
        let memory = MemoryArg {
            memory: self.memory,
            offset: 12,
            align: 2,
        };
        let previous = body.add_op(
            block,
            Operator::I32Load { memory },
            &[self.anchor],
            &[Type::I32],
        );
        body.add_op(block, Operator::I32Store { memory }, &[frame, pointer], &[]);
        body.add_op(
            block,
            Operator::I32Store {
                memory: MemoryArg {
                    offset: 16,
                    ..memory
                },
            },
            &[frame, previous],
            &[],
        );
        body.add_op(
            block,
            Operator::I32Store { memory },
            &[self.anchor, frame],
            &[],
        );
    }

    pub(crate) fn release(&self, body: &mut FunctionBody, block: Block) -> Block {
        let memory = MemoryArg {
            memory: self.memory,
            offset: 12,
            align: 2,
        };
        let head = body.add_op(
            block,
            Operator::I32Load { memory },
            &[self.anchor],
            &[Type::I32],
        );
        let check = body.add_block();
        let frame = body.add_blockparam(check, Type::I32);
        let drop = body.add_block();
        let done = body.add_block();
        body.set_terminator(
            block,
            Terminator::Br {
                target: BlockTarget {
                    block: check,
                    args: vec![head],
                },
            },
        );
        body.set_terminator(
            check,
            Terminator::CondBr {
                cond: frame,
                if_true: BlockTarget {
                    block: drop,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: done,
                    args: vec![],
                },
            },
        );
        let next = body.add_op(
            drop,
            Operator::I32Load {
                memory: MemoryArg {
                    offset: 16,
                    ..memory
                },
            },
            &[frame],
            &[Type::I32],
        );
        body.add_op(
            drop,
            Operator::Call {
                function_index: self.allocator.frame_drop,
            },
            &[frame],
            &[],
        );
        body.set_terminator(
            drop,
            Terminator::Br {
                target: BlockTarget {
                    block: check,
                    args: vec![next],
                },
            },
        );
        body.add_op(
            done,
            Operator::Call {
                function_index: self.allocator.frame_drop,
            },
            &[self.anchor],
            &[],
        );
        done
    }
}
