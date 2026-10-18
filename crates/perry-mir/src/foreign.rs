//! Statically bound component imports and their canonical transfer lifetime.
use crate::{
    NewOp,
    types::{PromiseType, TransferType},
};
use pliron::{
    builtin::attributes::StringAttr,
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_attr, pliron_op},
    op::Op,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};

#[pliron_attr(name = "perry.foreign_phase", format, verifier = "succ")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    Call,
    Start,
    Finish,
}

#[pliron_op(name = "perry.foreign",
    format = "attr($perry_foreign_phase, $Phase) ` ` attr($perry_foreign_binding, $StringAttr) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_foreign_binding: StringAttr, perry_foreign_phase: Phase))]
pub struct ForeignOp;

impl ForeignOp {
    pub fn new(
        ctx: &mut Context,
        phase: Phase,
        binding: &str,
        args: Vec<Value>,
        results: Vec<TypeHandle>,
    ) -> Self {
        let op = Self {
            op: Self::new_op(ctx, results, args),
        };
        op.set_attr_perry_foreign_binding(ctx, StringAttr::new(binding.to_owned()));
        op.set_attr_perry_foreign_phase(ctx, phase);
        op
    }
    pub fn binding(&self, ctx: &Context) -> String {
        self.get_attr_perry_foreign_binding(ctx)
            .expect("foreign binding")
            .as_str()
            .to_owned()
    }
    pub fn phase(&self, ctx: &Context) -> Phase {
        *self
            .get_attr_perry_foreign_phase(ctx)
            .expect("foreign phase")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("foreign call must have a binding and the signature required by its transfer phase")]
struct InvalidForeign;

impl Verify for ForeignOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let operation = self.get_operation().deref(ctx);
        let valid = match self.get_attr_perry_foreign_phase(ctx).as_deref() {
            Some(Phase::Call) => operation.get_num_results() <= 1,
            Some(Phase::Start) => {
                operation.get_num_results() == 1
                    && operation
                        .get_result(0)
                        .get_type(ctx)
                        .deref(ctx)
                        .downcast_ref::<PromiseType>()
                        .is_some_and(|promise| promise.value().deref(ctx).is::<TransferType>())
            }
            Some(Phase::Finish) => {
                operation.get_num_results() == 1
                    && operation.get_num_operands() == 1
                    && operation
                        .get_operand(0)
                        .get_type(ctx)
                        .deref(ctx)
                        .is::<TransferType>()
            }
            None => false,
        };
        if !valid
            || self
                .get_attr_perry_foreign_binding(ctx)
                .is_none_or(|binding| !binding.as_str().contains('#'))
        {
            verify_err!(self.loc(ctx), InvalidForeign)?;
        }
        Ok(())
    }
}

side_effects!(true; ForeignOp);

#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for ForeignOp {
    fn exception(&self, _: &Context) -> crate::attrs::ErrorKind {
        crate::attrs::ErrorKind::TypeError
    }
}
