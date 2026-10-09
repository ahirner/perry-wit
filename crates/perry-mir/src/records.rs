//! Record own-property operations retain presence independently of stored undefined values.
use crate::{
    attrs::ErrorKind,
    exceptions::MayThrowInterface,
    types::{ArrayType, RecordType, StringType},
};
use pliron::{
    builtin::{
        op_interfaces::OneResultInterface,
        types::{IntegerType, Signedness},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_attr, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};

#[pliron_attr(name = "perry.record_action", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Action {
    Keys,
    Has,
    Delete,
}

impl Action {
    fn result(self, ctx: &Context) -> TypeHandle {
        match self {
            Self::Keys => ArrayType::get(ctx, StringType::get(ctx).into()).into(),
            Self::Has | Self::Delete => IntegerType::get(ctx, 1, Signedness::Signless).into(),
        }
    }
}

#[pliron_op(name = "perry.record", format = "attr($perry_record_action, $Action) ` (` operands(CharSpace(`,`)) `) : ` type($0)", interfaces = [OneResultInterface], attributes = (perry_record_action: Action))]
pub struct AccessOp;
impl AccessOp {
    pub fn new(ctx: &mut Context, action: Action, args: Vec<Value>) -> Self {
        let result = action.result(ctx);
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                args,
                vec![],
                0,
            ),
        };
        op.set_attr_perry_record_action(ctx, action);
        op
    }
    pub fn action(&self, ctx: &Context) -> Action {
        *self
            .get_attr_perry_record_action(ctx)
            .expect("record action")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("record operation has incorrect operand or result types")]
struct SignatureError;

impl Verify for AccessOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        let action = self.action(ctx);
        if op.get_num_results() != 1
            || op.get_num_operands() != if action == Action::Keys { 1 } else { 2 }
            || !op
                .get_operand(0)
                .get_type(ctx)
                .deref(ctx)
                .is::<RecordType>()
            || action != Action::Keys
                && !op
                    .get_operand(1)
                    .get_type(ctx)
                    .deref(ctx)
                    .is::<StringType>()
            || op.get_result(0).get_type(ctx) != action.result(ctx)
        {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for AccessOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        true
    }
}
#[op_interface_impl]
impl MayThrowInterface for AccessOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
