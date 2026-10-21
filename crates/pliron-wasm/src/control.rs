//! Unstructured control flow with block arguments, structured into Wasm blocks by [crate::emit].
//!
//! Adapted from `pliron-llvm` (Apache-2.0, The pliron contributors).
use crate::types::val_type;
use core::ops::Range;
use pliron::{
    basic_block::BasicBlock,
    builtin::op_interfaces::{BranchOpInterface, IsTerminatorInterface, NResultsInterface},
    common_traits::Verify,
    context::{Context, Ptr},
    derive::{op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    result::Result,
    r#type::Typed,
    value::Value,
    verify_err,
};
use thiserror::Error;
use wasm_encoder::ValType;

mod branches {
    pliron_control::control_flow_ops!(
        "wasm.br",
        "wasm.cond_br",
        "wasm.return",
        |ctx, value: pliron::value::Value| {
            use pliron::r#type::Typed;
            crate::types::val_type(ctx, value.get_type(ctx)) == Some(wasm_encoder::ValType::I32)
        },
        "a signless i32"
    );
}
pub use branches::{BrOp, CondBrOp, ReturnOp};

/// Trap unconditionally and mark control-flow paths that the emitter proves unreachable.
#[pliron_op(
    name = "wasm.unreachable",
    format = "",
    interfaces = [IsTerminatorInterface, NResultsInterface<0>, pliron::builtin::op_interfaces::NOpdsInterface<0>],
    verifier = "succ"
)]
pub struct UnreachableOp;

impl UnreachableOp {
    pub fn new(ctx: &mut Context) -> Self {
        Operation::new_op::<Self>(ctx, vec![], vec![], vec![], 0)
    }
}

/// `br_table`: jump to `successor[index]`, or to the last successor (the default) when
/// `index` is out of range. Successors take no arguments (Wasm `br_table` passes the same
/// values to every target), so a target that needs some is reached through a block that
/// supplies them.
#[pliron_op(
    name = "wasm.br_table",
    format = "$0 ` [` successors(CharSpace(`,`)) `]`",
    interfaces = [IsTerminatorInterface, NResultsInterface<0>],
    operands = (index)
)]
pub struct BrTableOp;

impl BrTableOp {
    /// `targets[index]`, otherwise `default`.
    pub fn new(
        ctx: &mut Context,
        index: Value,
        targets: Vec<Ptr<BasicBlock>>,
        default: Ptr<BasicBlock>,
    ) -> Self {
        let mut successors = targets;
        successors.push(default);
        Operation::new_op::<Self>(ctx, vec![], vec![index], successors, 0)
    }
}

#[derive(Debug, Error)]
enum BrTableErr {
    #[error("wasm.br_table needs a default successor")]
    Default,
    #[error("wasm.br_table index must be a signless i32")]
    Index,
    #[error("wasm.br_table needs one index and no successor operands, found {0} operands")]
    Operands(usize),
}

impl Verify for BrTableOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        self.verify_successor_operand_layout(ctx)?;
        let operation = self.get_operation().deref(ctx);
        if val_type(ctx, operation.get_operand(0).get_type(ctx)) != Some(ValType::I32) {
            verify_err!(self.loc(ctx), BrTableErr::Index)?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl BranchOpInterface for BrTableOp {
    fn verify_successor_operand_layout(&self, ctx: &Context) -> Result<()> {
        let operation = self.get_operation().deref(ctx);
        if operation.get_num_operands() != 1 {
            verify_err!(
                self.loc(ctx),
                BrTableErr::Operands(operation.get_num_operands())
            )?
        }
        if operation.successors().next().is_none() {
            verify_err!(self.loc(ctx), BrTableErr::Default)?
        }
        Ok(())
    }

    fn successor_operand_range(&self, _ctx: &Context, _succ_idx: usize) -> Range<usize> {
        1..1
    }

    fn add_successor_operand(&self, _ctx: &mut Context, _: usize, _: Value) -> usize {
        panic!("wasm.br_table successors take no operands")
    }

    fn remove_successor_operand(&self, _ctx: &mut Context, _: usize, _: usize) -> Value {
        panic!("wasm.br_table successors take no operands")
    }
}
