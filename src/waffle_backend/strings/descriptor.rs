//! Shared allocation and field layout for runtime string descriptors.

use waffle::{Block, Func, FunctionBody, Memory, MemoryArg, Operator, Type, Value};

pub(super) struct StringDescriptor {
    pub(super) data_ptr: Value,
    pub(super) byte_len: Value,
    pub(super) scalar_len: Value,
}

impl StringDescriptor {
    /// Allocates a descriptor without copying the referenced UTF-8 bytes.
    pub(super) fn allocate(
        self,
        body: &mut FunctionBody,
        block: Block,
        memory: Memory,
        allocator: Func,
    ) -> Value {
        let zero = body.add_op(block, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let align = body.add_op(block, Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let size = body.add_op(block, Operator::I32Const { value: 12 }, &[], &[Type::I32]);
        let address = body.add_op(
            block,
            Operator::Call {
                function_index: allocator,
            },
            &[zero, zero, align, size],
            &[Type::I32],
        );
        crate::waffle_backend::allocation::tag_allocation(
            body,
            block,
            memory,
            address,
            crate::waffle_backend::allocation::AllocationKind::String,
        );
        for (offset, value) in [(0, self.data_ptr), (4, self.byte_len), (8, self.scalar_len)] {
            body.add_op(
                block,
                Operator::I32Store {
                    memory: MemoryArg {
                        align: 2,
                        offset,
                        memory,
                    },
                },
                &[address, value],
                &[],
            );
        }
        address
    }
}
