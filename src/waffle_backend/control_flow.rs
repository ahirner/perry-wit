//! SSA control-flow construction helpers.
//!
//! Owns block parameters, binding snapshots, and branch argument construction,
//! ensuring that control-flow joins maintain consistent SSA forms without
//! requiring access to the entire mutable lowerer or redundant cloning.

use perry_hir::types::LocalId;
use std::collections::BTreeMap;
use waffle::{Block, BlockTarget, FunctionBody, Terminator, Value};

/// Creates SSA block parameters on `block` corresponding to the given local bindings.
pub(crate) fn create_block_parameters(
    body: &mut FunctionBody,
    block: Block,
    template_locals: &BTreeMap<LocalId, Value>,
) -> BTreeMap<LocalId, Value> {
    template_locals
        .iter()
        .map(|(&id, &val)| {
            let ty = body.values[val]
                .ty(&body.type_pool)
                .expect("Local binding must have a valid type");
            let param = body.add_blockparam(block, ty);
            (id, param)
        })
        .collect()
}

/// A target block in SSA control flow with block parameters representing
/// live local variables at the merge point.
pub(crate) struct JoinPoint {
    pub(crate) block: Block,
    pub(crate) bindings: BTreeMap<LocalId, Value>,
}

impl JoinPoint {
    /// Creates a new block with a description and block parameters matching `template_locals`.
    pub(crate) fn new(
        body: &mut FunctionBody,
        desc: &str,
        template_locals: &BTreeMap<LocalId, Value>,
    ) -> Self {
        let block = body.add_block();
        body.blocks[block].desc = desc.into();
        let bindings = create_block_parameters(body, block, template_locals);
        Self { block, bindings }
    }

    /// Computes branch arguments from the current local environment in parameter order.
    pub(crate) fn branch_args(&self, current_locals: &BTreeMap<LocalId, Value>) -> Vec<Value> {
        self.bindings.keys().map(|id| current_locals[id]).collect()
    }

    /// Emits an unconditional branch from `from_block` to this join point.
    pub(crate) fn emit_branch(
        &self,
        body: &mut FunctionBody,
        from_block: Block,
        current_locals: &BTreeMap<LocalId, Value>,
    ) {
        let args = self.branch_args(current_locals);
        body.set_terminator(
            from_block,
            Terminator::Br {
                target: BlockTarget {
                    block: self.block,
                    args,
                },
            },
        );
    }
}
