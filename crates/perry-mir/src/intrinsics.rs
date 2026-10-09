//! Typed host capabilities. Signatures are verified here; native representations and
//! task scheduling belong to codegen and the runtime.
use crate::types::{ArrayType, OptionalType, StringType};
use pliron::{
    builtin::types::FP64Type,
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
use thiserror::Error;

#[pliron_attr(name = "perry.intrinsic_kind", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Intrinsic {
    Stdout,
    Stderr,
    EnvGet,
    EnvKeys,
    Arguments,
    MonotonicNow,
    DateNow,
    ExitCodeGet,
    ExitCodeSet,
    ExitCodeClear,
    ProcessExit,
    ProcessExitCurrent,
}

#[derive(Clone, Copy)]
enum ValueType {
    String,
    OptionalString,
    OptionalNumber,
    Strings,
    Number,
}

impl ValueType {
    fn get(self, ctx: &mut Context) -> TypeHandle {
        match self {
            Self::Number => FP64Type::get(ctx).into(),
            Self::String => StringType::get(ctx).into(),
            Self::OptionalString | Self::OptionalNumber => {
                let value = if matches!(self, Self::OptionalString) {
                    StringType::get(ctx).into()
                } else {
                    FP64Type::get(ctx).into()
                };
                OptionalType::get(ctx, value).into()
            }
            Self::Strings => {
                let string = StringType::get(ctx).into();
                ArrayType::get(ctx, string).into()
            }
        }
    }
    fn matches(self, ctx: &Context, ty: TypeHandle) -> bool {
        match self {
            Self::Number => ty.deref(ctx).is::<FP64Type>(),
            Self::String => ty.deref(ctx).is::<StringType>(),
            Self::OptionalString => ty
                .deref(ctx)
                .downcast_ref::<OptionalType>()
                .is_some_and(|ty| ty.value().deref(ctx).is::<StringType>()),
            Self::OptionalNumber => ty
                .deref(ctx)
                .downcast_ref::<OptionalType>()
                .is_some_and(|ty| ty.value().deref(ctx).is::<FP64Type>()),
            Self::Strings => ty
                .deref(ctx)
                .downcast_ref::<ArrayType>()
                .is_some_and(|ty| ty.element().deref(ctx).is::<StringType>()),
        }
    }
}

impl Intrinsic {
    fn signature(self) -> (&'static [ValueType], &'static [ValueType]) {
        use ValueType::*;
        match self {
            Self::ExitCodeGet => (&[], &[OptionalNumber]),
            Self::ExitCodeSet => (&[Number], &[Number]),
            Self::ExitCodeClear | Self::ProcessExitCurrent => (&[], &[]),
            Self::ProcessExit => (&[Number], &[]),
            Self::Stdout | Self::Stderr => (&[String], &[]),
            Self::EnvGet => (&[String], &[OptionalString]),
            Self::EnvKeys | Self::Arguments => (&[], &[Strings]),
            Self::MonotonicNow | Self::DateNow => (&[], &[Number]),
        }
    }
}

#[derive(Debug, Error)]
#[error("intrinsic {0:?} has incorrect operand or result types")]
struct SignatureError(Intrinsic);

#[pliron_op(
    name = "perry.intrinsic",
    format = "attr($perry_intrinsic, $Intrinsic) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_intrinsic: Intrinsic)
)]
pub struct IntrinsicOp;

impl IntrinsicOp {
    pub fn new(ctx: &mut Context, kind: Intrinsic, args: Vec<Value>) -> Self {
        let results = kind.signature().1.iter().map(|ty| ty.get(ctx)).collect();
        let op = Self {
            op: Operation::new(ctx, Self::get_concrete_op_info(), results, args, vec![], 0),
        };
        op.set_attr_perry_intrinsic(ctx, kind);
        op
    }
    pub fn kind(&self, ctx: &Context) -> Intrinsic {
        *self.get_attr_perry_intrinsic(ctx).expect("intrinsic kind")
    }
}

impl Verify for IntrinsicOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let kind = self.kind(ctx);
        let (params, results) = kind.signature();
        let op = self.get_operation().deref(ctx);
        if op.get_num_operands() != params.len()
            || op.get_num_results() != results.len()
            || !params
                .iter()
                .zip(op.operands())
                .all(|(ty, v)| ty.matches(ctx, v.get_type(ctx)))
            || !results
                .iter()
                .zip(op.results())
                .all(|(ty, v)| ty.matches(ctx, v.get_type(ctx)))
        {
            verify_err!(self.loc(ctx), SignatureError(kind))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for IntrinsicOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        true
    }
}

#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for IntrinsicOp {
    fn exception(&self, _ctx: &Context) -> crate::attrs::ErrorKind {
        crate::attrs::ErrorKind::RangeError
    }
    fn may_throw(&self, ctx: &Context) -> bool {
        matches!(
            self.kind(ctx),
            Intrinsic::ExitCodeSet | Intrinsic::ProcessExit
        )
    }
}
