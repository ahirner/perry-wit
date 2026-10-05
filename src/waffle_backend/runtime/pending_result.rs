//! One owner carries an async export's source graph and canonical storage to task.return.
use super::builder::Builder;
use crate::waffle_backend::allocation::{AllocationFuncs, scope::ScratchScope};
use waffle::{Block, FunctionBody, Memory, MemoryArg, Operator, Type};

const OWNER: u32 = 140;

pub(crate) struct PendingExportResult {
    retained: ScratchScope,
}

impl PendingExportResult {
    pub(crate) fn new(retained: ScratchScope) -> Self {
        Self { retained }
    }

    /// Transfers the complete result lifetime from the adapter to the callback.
    pub(crate) fn handoff(self, body: &mut FunctionBody, block: Block, memory: Memory) {
        let address = body.add_op(
            block,
            Operator::I32Const { value: OWNER },
            &[],
            &[Type::I32],
        );
        body.add_op(
            block,
            Operator::I32Store {
                memory: MemoryArg {
                    memory,
                    offset: 0,
                    align: 2,
                },
            },
            &[address, self.retained.into_anchor()],
            &[],
        );
    }

    /// Called only after task.return or acknowledged cancellation; clears ownership once.
    pub(crate) fn release(b: &mut Builder, allocator: AllocationFuncs, memory: Memory) {
        let address = b.integer(OWNER);
        let anchor = b.load(address, 0, Type::I32);
        let owned = b.body.add_block();
        let done = b.body.add_block();
        b.branch(anchor, owned, done);
        b.block = owned;
        let zero = b.integer(0);
        b.store(address, 0, zero, Type::I32);
        let scope = ScratchScope::from_anchor(anchor, memory, allocator);
        b.block = scope.release(&mut b.body, b.block);
        b.jump(done, &[]);
        b.block = done;
    }

    pub(crate) fn require_vacant(b: &mut Builder) {
        let address = b.integer(OWNER);
        let owner = b.load(address, 0, Type::I32);
        let empty = b.op(Operator::I32Eqz, &[owner], Type::I32);
        b.require(empty);
    }
}
