//! Host and runtime services with fixed signatures (`perry.intrinsic`).
//!
//! The builder recognizes the standard API (`console.log`, `process.env`, `performance.now`
//! and so on) and emits one op per service, named by [Intrinsic]; codegen maps each to the
//! runtime function that implements it and checks that the WIT world imports what it needs.
use crate::types::{ArrayType, OptionalType, StringType};
use pliron::{
    builtin::{attributes::IdentifierAttr, types::FP64Type},
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    identifier::Identifier,
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};
use thiserror::Error;

/// The services, with their operand and result types.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Intrinsic {
    /// `console.log` (string) → (): one line to stdout.
    PrintOut,
    /// `console.error` (string) → (): one line to stderr.
    PrintErr,
    /// `String(number)` (f64) → string.
    NumberToString,
    /// `process.env.NAME` (string) → string | undefined.
    EnvGet,
    /// `Object.keys(process.env)` () → string[].
    EnvKeys,
    /// `performance.now()` () → f64.
    PerformanceNow,
    /// `Date.now()` () → f64.
    DateNow,
    /// `process.exitCode = code` (f64) → ().
    ExitCodeSet,
}

impl Intrinsic {
    pub const ALL: [Intrinsic; 8] = [
        Intrinsic::PrintOut,
        Intrinsic::PrintErr,
        Intrinsic::NumberToString,
        Intrinsic::EnvGet,
        Intrinsic::EnvKeys,
        Intrinsic::PerformanceNow,
        Intrinsic::DateNow,
        Intrinsic::ExitCodeSet,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Intrinsic::PrintOut => "print_out",
            Intrinsic::PrintErr => "print_err",
            Intrinsic::NumberToString => "number_to_string",
            Intrinsic::EnvGet => "env_get",
            Intrinsic::EnvKeys => "env_keys",
            Intrinsic::PerformanceNow => "performance_now",
            Intrinsic::DateNow => "date_now",
            Intrinsic::ExitCodeSet => "exit_code_set",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.name() == name)
    }

    /// (operands, results).
    pub fn signature(self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let string: TypeHandle = StringType::get(ctx).into();
        let number: TypeHandle = FP64Type::get(ctx).into();
        match self {
            Intrinsic::PrintOut | Intrinsic::PrintErr => (vec![string], vec![]),
            Intrinsic::NumberToString => (vec![number], vec![string]),
            Intrinsic::EnvGet => (vec![string], vec![OptionalType::get(ctx, string).into()]),
            Intrinsic::EnvKeys => (vec![], vec![ArrayType::get(ctx, string).into()]),
            Intrinsic::PerformanceNow | Intrinsic::DateNow => (vec![], vec![number]),
            Intrinsic::ExitCodeSet => (vec![number], vec![]),
        }
    }

    /// Pure services can be removed when unused.
    pub fn pure(self) -> bool {
        matches!(self, Intrinsic::NumberToString)
    }
}

#[derive(Debug, Error)]
pub enum IntrinsicErr {
    #[error("unknown intrinsic {0}")]
    Unknown(String),
    #[error("intrinsic {0} has the wrong operand or result types")]
    Signature(String),
}

/// A call of a host or runtime service.
#[pliron_op(
    name = "perry.intrinsic",
    format = "attr($perry_intrinsic, $IdentifierAttr) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_intrinsic: IdentifierAttr)
)]
pub struct IntrinsicOp;

impl IntrinsicOp {
    pub fn new(ctx: &mut Context, intrinsic: Intrinsic, args: Vec<Value>) -> Self {
        let (_, results) = intrinsic.signature(ctx);
        let op = IntrinsicOp {
            op: Operation::new(ctx, Self::get_concrete_op_info(), results, args, vec![], 0),
        };
        let name: Identifier = intrinsic.name().try_into().expect("identifier");
        op.set_attr_perry_intrinsic(ctx, IdentifierAttr::new(name));
        op
    }

    pub fn intrinsic(&self, ctx: &Context) -> Option<Intrinsic> {
        let name: Identifier = self.get_attr_perry_intrinsic(ctx)?.clone().into();
        Intrinsic::from_name(name.as_ref())
    }
}

impl Verify for IntrinsicOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let Some(intrinsic) = self.intrinsic(ctx) else {
            let name = format!(
                "{:?}",
                self.get_attr_perry_intrinsic(ctx).map(|a| a.clone())
            );
            return verify_err!(self.loc(ctx), IntrinsicErr::Unknown(name));
        };
        let op = self.get_operation().deref(ctx);
        let operands: Vec<_> = op.operands().map(|v| v.get_type(ctx)).collect();
        let results: Vec<_> = op.results().map(|v| v.get_type(ctx)).collect();
        drop(op);
        // Signatures only use interned types; compare by structure without a mutable context.
        let expected = signature_matches(ctx, intrinsic, &operands, &results);
        if !expected {
            verify_err!(
                self.loc(ctx),
                IntrinsicErr::Signature(intrinsic.name().into())
            )?
        }
        Ok(())
    }
}

fn signature_matches(
    ctx: &Context,
    intrinsic: Intrinsic,
    operands: &[TypeHandle],
    results: &[TypeHandle],
) -> bool {
    let is = |ty: &TypeHandle, kind: &str| -> bool {
        let deref = ty.deref(ctx);
        match kind {
            "string" => deref.is::<StringType>(),
            "number" => deref.is::<FP64Type>(),
            "optional_string" => deref
                .downcast_ref::<OptionalType>()
                .is_some_and(|o| o.value().deref(ctx).is::<StringType>()),
            "string_array" => deref
                .downcast_ref::<ArrayType>()
                .is_some_and(|a| a.element().deref(ctx).is::<StringType>()),
            _ => false,
        }
    };
    let (ins, outs): (&[&str], &[&str]) = match intrinsic {
        Intrinsic::PrintOut | Intrinsic::PrintErr => (&["string"], &[]),
        Intrinsic::NumberToString => (&["number"], &["string"]),
        Intrinsic::EnvGet => (&["string"], &["optional_string"]),
        Intrinsic::EnvKeys => (&[], &["string_array"]),
        Intrinsic::PerformanceNow | Intrinsic::DateNow => (&[], &["number"]),
        Intrinsic::ExitCodeSet => (&["number"], &[]),
    };
    operands.len() == ins.len()
        && results.len() == outs.len()
        && operands.iter().zip(ins).all(|(t, k)| is(t, k))
        && results.iter().zip(outs).all(|(t, k)| is(t, k))
}

#[op_interface_impl]
impl SideEffects for IntrinsicOp {
    fn has_side_effects(&self, ctx: &Context) -> bool {
        !self.intrinsic(ctx).is_some_and(Intrinsic::pure)
    }
}
