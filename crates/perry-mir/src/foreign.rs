//! Statically bound component imports and their canonical transfer lifetime.
use crate::operation_schema::op_schema;
use crate::signatures::{OutputContract, Signature};
use crate::types::{PromiseType, TransferType};
use pliron::{
    builtin::attributes::StringAttr, common_traits::Verify, context::Context, derive::pliron_attr,
    op::Op, operation::Operation, result::Result, r#type::TypeHandle, value::Value,
};

#[pliron_attr(name = "perry.foreign_phase", format, verifier = "succ")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    Call,
    Start,
    Finish,
}

op_schema! {
    pub ForeignOp(.._args) -> infallible;
    name = "perry.foreign";
    format = "attr($perry_foreign_phase, $Phase) ` ` attr($perry_foreign_binding, $StringAttr) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))";
    attributes = (perry_foreign_binding: StringAttr, perry_foreign_phase: Phase);
    constructors {
        pub new(phase: Phase, binding: &str, args: Vec<Value>, results: Vec<TypeHandle>)
            operands(args) => results(results);
    }
    attribute(phase: Phase, get_attr_perry_foreign_phase, set_attr_perry_foreign_phase) =
        phase => InvalidForeign;
    attribute(binding: String, get_attr_perry_foreign_binding, set_attr_perry_foreign_binding) =
        StringAttr::new(binding.to_owned()) => InvalidForeign;
    prepare(_this) { let phase = *phase; }
    result(ctx, phase: Phase): Phase = phase;
    error = InvalidForeign;
    check binding.as_str().contains('#') => InvalidForeign;
}

#[derive(Debug, thiserror::Error)]
#[error("foreign call must have a binding and the signature required by its transfer phase")]
struct InvalidForeign;

impl OutputContract for Phase {
    fn accepts(self, ctx: &Context, op: &Operation) -> bool {
        match self {
            Phase::Call => op.get_num_results() <= 1,
            Phase::Start => {
                let transfer = TransferType::get(ctx).into();
                let promise: TypeHandle = PromiseType::get(ctx, transfer).into();
                promise.accepts(ctx, op)
            }
            Phase::Finish => {
                op.get_num_results() == 1
                    && (Signature {
                        inputs: [TypeHandle::from(TransferType::get(ctx))],
                        outputs: crate::signatures::result_types(ctx, op),
                    })
                    .accepts(ctx, op)
            }
        }
    }
}
