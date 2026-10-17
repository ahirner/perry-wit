//! Mutable locals as slots, promoted to SSA values and block arguments by pliron `mem2reg`.
//!
//! The HIR→MIR builder (P2.2) emits one `perry.local` per mutable binding and plain loads and
//! stores, so it never constructs SSA joins, loop-carried block parameters or await
//! continuations itself; that hand-rolled machinery in the legacy `ssa.rs` goes away.
use crate::types::SlotType;
use pliron::{
    builtin::op_interfaces::{
        NOpdsInterface, NResultsInterface, OneOpdInterface, OneResultInterface,
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    irbuild::{inserter::Inserter, rewriter::Rewriter},
    op::Op,
    operation::Operation,
    opts::{
        dce::SideEffects,
        mem2reg::{
            AllocInfo, PromotableAllocationInterface, PromotableOpInterface, PromotableOpKind,
        },
    },
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum LocalErr {
    #[error("{0} expects a perry.slot operand")]
    NotSlot(&'static str),
    #[error("{0} value type differs from the slot's type")]
    Type(&'static str),
    #[error("allocation info does not belong to this op")]
    Unrelated,
}

fn slot_value(ctx: &Context, slot: Value, op: &'static str) -> Result<TypeHandle> {
    match TypedHandle::<SlotType>::from_handle(slot.get_type(ctx), ctx) {
        Ok(slot) => Ok(slot.deref(ctx).value()),
        Err(_) => verify_err!(pliron::location::Location::Unknown, LocalErr::NotSlot(op)),
    }
}

/// A value of the given type whose content is never observed (reads before any write).
#[pliron_op(
    name = "perry.undef",
    format = "` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface],
    verifier = "succ"
)]
pub struct UndefOp;

impl UndefOp {
    pub fn new(ctx: &mut Context, ty: TypeHandle) -> Self {
        UndefOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![],
                vec![],
                0,
            ),
        }
    }
}

#[op_interface_impl]
impl SideEffects for UndefOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Storage for one mutable local binding.
#[pliron_op(
    name = "perry.local",
    format = "` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface],
)]
pub struct LocalOp;

impl LocalOp {
    pub fn new(ctx: &mut Context, value: TypeHandle) -> Self {
        let slot = SlotType::get(ctx, value).into();
        LocalOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![slot],
                vec![],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for LocalOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        slot_value(ctx, self.get_result(ctx), "perry.local").map(|_| ())
    }
}

#[op_interface_impl]
impl PromotableAllocationInterface for LocalOp {
    fn alloc_info(&self, ctx: &Context) -> Vec<AllocInfo> {
        let ptr = self.get_result(ctx);
        let ty = slot_value(ctx, ptr, "perry.local").expect("verified slot");
        vec![AllocInfo { ptr, ty }]
    }

    fn default_value(
        &self,
        ctx: &mut Context,
        inserter: &mut dyn Inserter,
        alloc_info: &AllocInfo,
    ) -> Result<Value> {
        let undef = UndefOp::new(ctx, alloc_info.ty);
        let value = undef.get_result(ctx);
        inserter.insert_op(ctx, &undef);
        Ok(value)
    }

    fn promote(
        &self,
        ctx: &mut Context,
        rewriter: &mut dyn Rewriter,
        alloc_infos: &[AllocInfo],
    ) -> Result<()> {
        if alloc_infos.len() != 1 || alloc_infos[0].ptr != self.get_result(ctx) {
            verify_err!(self.loc(ctx), LocalErr::Unrelated)?
        }
        rewriter.erase_operation(ctx, self.get_operation());
        Ok(())
    }
}

/// Read a local.
#[pliron_op(
    name = "perry.load",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface],
)]
pub struct LoadOp;

impl LoadOp {
    pub fn new(ctx: &mut Context, slot: Value) -> Result<Self> {
        let ty = slot_value(ctx, slot, "perry.load")?;
        Ok(LoadOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![slot],
                vec![],
                0,
            ),
        })
    }

    pub(crate) fn slot(&self, ctx: &Context) -> Value {
        self.get_operation().deref(ctx).get_operand(0)
    }
}

impl Verify for LoadOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if slot_value(ctx, self.slot(ctx), "perry.load")? != self.get_result(ctx).get_type(ctx) {
            verify_err!(self.loc(ctx), LocalErr::Type("perry.load"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for LoadOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

#[op_interface_impl]
impl PromotableOpInterface for LoadOp {
    fn promotion_kind(&self, ctx: &Context, alloc_info: &AllocInfo) -> PromotableOpKind {
        if self.slot(ctx) == alloc_info.ptr {
            PromotableOpKind::Load
        } else {
            PromotableOpKind::NonPromotableUse
        }
    }

    fn promote(
        &self,
        ctx: &mut Context,
        reaching: &[(AllocInfo, Value)],
        rewriter: &mut dyn Rewriter,
    ) -> Result<()> {
        let [(info, value)] = reaching else {
            return verify_err!(self.loc(ctx), LocalErr::Unrelated);
        };
        if self.slot(ctx) != info.ptr {
            verify_err!(self.loc(ctx), LocalErr::Unrelated)?
        }
        rewriter.replace_operation_with_values(ctx, self.get_operation(), vec![*value]);
        Ok(())
    }
}

/// Write a local.
#[pliron_op(
    name = "perry.store",
    format = "$0 ` = ` $1",
    interfaces = [NOpdsInterface<2>, NResultsInterface<0>],
)]
pub struct StoreOp;

impl StoreOp {
    pub fn new(ctx: &mut Context, slot: Value, value: Value) -> Self {
        StoreOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![slot, value],
                vec![],
                0,
            ),
        }
    }

    pub(crate) fn slot(&self, ctx: &Context) -> Value {
        self.get_operation().deref(ctx).get_operand(0)
    }

    pub(crate) fn value(&self, ctx: &Context) -> Value {
        self.get_operation().deref(ctx).get_operand(1)
    }
}

impl Verify for StoreOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if slot_value(ctx, self.slot(ctx), "perry.store")? != self.value(ctx).get_type(ctx) {
            verify_err!(self.loc(ctx), LocalErr::Type("perry.store"))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl PromotableOpInterface for StoreOp {
    fn promotion_kind(&self, ctx: &Context, alloc_info: &AllocInfo) -> PromotableOpKind {
        if self.slot(ctx) == alloc_info.ptr {
            PromotableOpKind::Store(self.value(ctx))
        } else {
            PromotableOpKind::NonPromotableUse
        }
    }

    fn promote(
        &self,
        ctx: &mut Context,
        reaching: &[(AllocInfo, Value)],
        rewriter: &mut dyn Rewriter,
    ) -> Result<()> {
        let [(info, _)] = reaching else {
            return verify_err!(self.loc(ctx), LocalErr::Unrelated);
        };
        if self.slot(ctx) != info.ptr {
            verify_err!(self.loc(ctx), LocalErr::Unrelated)?
        }
        rewriter.erase_operation(ctx, self.get_operation());
        Ok(())
    }
}
