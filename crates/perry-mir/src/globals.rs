//! Typed module cells. Local slots promote to SSA; module cells persist across export calls.
use crate::types::SlotType;
use pliron::{
    builtin::{
        attributes::IdentifierAttr,
        op_interfaces::{NOpdsInterface, OneResultInterface},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    identifier::Identifier,
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};

pub const INITIALIZER: &str = "perry_module_init";

/// Address of a statically typed persistent cell, initialized by the module initializer.
#[pliron_op(
    name = "perry.global",
    format = "attr($perry_global, $IdentifierAttr) ` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface],
    attributes = (perry_global: IdentifierAttr)
)]
pub struct GlobalOp;

impl GlobalOp {
    pub fn new(ctx: &mut Context, name: Identifier, value: TypeHandle) -> Self {
        let slot = SlotType::get(ctx, value).into();
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![slot],
                vec![],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_global(ctx, IdentifierAttr::new(name));
        op
    }

    pub fn name(&self, ctx: &Context) -> Identifier {
        self.get_attr_perry_global(ctx)
            .expect("global name")
            .clone()
            .into()
    }
}

impl Verify for GlobalOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        TypedHandle::<SlotType>::from_handle(self.get_result(ctx).get_type(ctx), ctx).map(|_| ())
    }
}

#[op_interface_impl]
impl SideEffects for GlobalOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Checked load; reading a module binding before its initializer ran raises a TypeError.
#[pliron_op(name = "perry.global_load", format = "$0 ` : ` type($0)", interfaces = [NOpdsInterface<1>, OneResultInterface])]
pub struct GlobalLoadOp;

impl GlobalLoadOp {
    pub fn new(ctx: &mut Context, slot: Value) -> Result<Self> {
        let ty = TypedHandle::<SlotType>::from_handle(slot.get_type(ctx), ctx)?
            .deref(ctx)
            .value();
        Ok(Self {
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
}

impl Verify for GlobalLoadOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let slot = self.get_operation().deref(ctx).get_operand(0);
        let expected = TypedHandle::<SlotType>::from_handle(slot.get_type(ctx), ctx)?
            .deref(ctx)
            .value();
        if expected != self.get_result(ctx).get_type(ctx) {
            pliron::verify_err!(
                self.loc(ctx),
                crate::locals::LocalErr::Type("perry.global_load")
            )?;
        }
        Ok(())
    }
}

#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for GlobalLoadOp {
    fn exception(&self, _ctx: &Context) -> crate::attrs::ErrorKind {
        crate::attrs::ErrorKind::TypeError
    }
}
