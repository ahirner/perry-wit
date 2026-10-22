//! Mutable locals as slots, promoted to SSA values and block arguments by pliron `mem2reg`.
//!
//! The HIR-to-MIR builder emits one `perry.local` per mutable binding together with `perry.load`
//! and `perry.store` operations. The `mem2reg` pass then constructs SSA joins and loop-carried block parameters.
use crate::operation_schema::op_schema;
use crate::signatures::{Accepts, OutputContract};
use crate::types::SlotType;
use pliron::{
    builtin::op_interfaces::{NOpdsInterface, OneResultInterface},
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    irbuild::{inserter::Inserter, rewriter::Rewriter},
    op::Op,
    operation::Operation,
    opts::mem2reg::{
        AllocInfo, PromotableAllocationInterface, PromotableOpInterface, PromotableOpKind,
    },
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};

#[derive(Debug)]
pub(crate) enum LocalErr {
    NotSlot(&'static str),
    Type(&'static str),
    Unrelated,
}

impl core::fmt::Display for LocalErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotSlot(op) => write!(f, "{op} expects a perry.slot operand"),
            Self::Type(op) => write!(f, "{op} value type differs from the slot's type"),
            Self::Unrelated => f.write_str("allocation info does not belong to this op"),
        }
    }
}

impl core::error::Error for LocalErr {}

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
        Operation::new_op::<Self>(ctx, vec![ty], vec![], vec![], 0)
    }
}

op_schema! {
    /// Storage for one mutable local binding.
    pub LocalOp() -> infallible;
    name = "perry.local";
    format = "` : ` type($0)";
    constructors { pub new(value: TypeHandle) => explicit(SlotType::get(ctx, value).into()); }
    result(ctx): impl OutputContract = {
        Accepts(|ctx: &Context, ty: TypeHandle| ty.deref(ctx).is::<SlotType>())
    };
    error = LocalErr::NotSlot("perry.local");
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

op_schema! {
    /// Read a local.
    pub LoadOp(slot) -> fallible;
    name = "perry.load";
    format = "$0 ` : ` type($0)";
    constructors { pub new(slot: Value) => infer(slot); }
    result(ctx, slot: Value): TypeHandle = slot_value(ctx, slot, "perry.load");
    error = LocalErr::Type("perry.load");
}

impl LoadOp {
    pub(crate) fn slot(&self, ctx: &Context) -> Value {
        self.get_operation().deref(ctx).get_operand(0)
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

op_schema! {
    /// Write a local.
    pub StoreOp(slot, value) -> infallible;
    name = "perry.store";
    format = "$0 ` = ` $1";
    constructors { pub new(slot: Value, value: Value) => empty(); }
    result(ctx): () = ();
    error = LocalErr::Type("perry.store");
    check slot_value(ctx, slot, "perry.store")? == value.get_type(ctx)
        => LocalErr::Type("perry.store");
}

impl StoreOp {
    pub(crate) fn slot(&self, ctx: &Context) -> Value {
        self.get_operation().deref(ctx).get_operand(0)
    }

    pub(crate) fn value(&self, ctx: &Context) -> Value {
        self.get_operation().deref(ctx).get_operand(1)
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
