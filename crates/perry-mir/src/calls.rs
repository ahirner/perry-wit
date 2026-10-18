//! Direct calls and async calls to functions of the enclosing module.
//!
//! Perry has no first-class functions in MIR (subset rule R5): every callee is a symbol,
//! which keeps escape summaries and inlining (P3.2, P3.3) exact.
use crate::{NewOp, types::PromiseType};
use pliron::{
    builtin::{
        attributes::IdentifierAttr,
        op_interfaces::{OneResultInterface, SymbolTableInterface},
        ops::FuncOp,
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    common_traits::Verify,
    context::{Context, Ptr},
    derive::{op_interface_impl, pliron_op},
    identifier::Identifier,
    op::{Op, op_cast},
    operation::Operation,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CallErr {
    #[error("callee {0} is not a function of the enclosing module")]
    Unknown(String),
    #[error("call to {0} does not match the callee's signature")]
    Signature(String),
    #[error("perry.await operand must be a perry.promise of the result type")]
    Await,
}

/// Signature `(arguments, results)` of the module-level function `callee` seen from `op`.
fn callee_signature(
    ctx: &Context,
    op: Ptr<Operation>,
    callee: &Identifier,
) -> Result<(Vec<TypeHandle>, Vec<TypeHandle>)> {
    let mut parent = op.deref(ctx).get_parent_op(ctx);
    while let Some(candidate) = parent {
        let candidate_op = Operation::get_op_dyn(candidate, ctx);
        if let Some(table) = op_cast::<dyn SymbolTableInterface>(candidate_op.as_ref())
            && let Some(function) = table.lookup(ctx, callee)
            && let Some(function) = Operation::get_op::<FuncOp>(function, ctx)
        {
            let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)?;
            let ty = ty.deref(ctx);
            return Ok((ty.arg_types(), ty.res_types()));
        }
        parent = candidate.deref(ctx).get_parent_op(ctx);
    }
    let loc = Operation::get_op_dyn(op, ctx).loc(ctx);
    verify_err!(loc, CallErr::Unknown(callee.to_string()))
}

/// The function `op` calls, synchronously or asynchronously; every MIR callee is a symbol.
pub(crate) fn callee(ctx: &Context, op: Ptr<Operation>) -> Option<Identifier> {
    Operation::get_op::<CallOp>(op, ctx)
        .map(|call| call.callee(ctx))
        .or_else(|| Operation::get_op::<AsyncCallOp>(op, ctx).map(|call| call.callee(ctx)))
}

fn operand_types(ctx: &Context, op: Ptr<Operation>) -> Vec<TypeHandle> {
    op.deref(ctx).operands().map(|v| v.get_type(ctx)).collect()
}

fn result_types(ctx: &Context, op: Ptr<Operation>) -> Vec<TypeHandle> {
    op.deref(ctx).results().map(|v| v.get_type(ctx)).collect()
}

/// Synchronous call; results are the callee's results.
#[pliron_op(
    name = "perry.call",
    format = "attr($perry_callee, $IdentifierAttr) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_callee: IdentifierAttr)
)]
pub struct CallOp;

impl CallOp {
    pub fn new(
        ctx: &mut Context,
        callee: Identifier,
        args: Vec<Value>,
        results: Vec<TypeHandle>,
    ) -> Self {
        let op = CallOp {
            op: Self::new_op(ctx, results, args),
        };
        op.set_attr_perry_callee(ctx, IdentifierAttr::new(callee));
        op
    }

    pub fn callee(&self, ctx: &Context) -> Identifier {
        self.get_attr_perry_callee(ctx)
            .expect("callee")
            .clone()
            .into()
    }
}

impl Verify for CallOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation();
        let callee = self.callee(ctx);
        let (args, results) = callee_signature(ctx, op, &callee)?;
        if args != operand_types(ctx, op) || results != result_types(ctx, op) {
            verify_err!(self.loc(ctx), CallErr::Signature(callee.to_string()))?
        }
        Ok(())
    }
}

/// Start an async function eagerly; it runs until its first pending `await` (D5).
#[pliron_op(
    name = "perry.async_call",
    format = "attr($perry_async_callee, $IdentifierAttr) ` (` operands(CharSpace(`,`)) `) : ` type($0)",
    interfaces = [OneResultInterface],
    attributes = (perry_async_callee: IdentifierAttr)
)]
pub struct AsyncCallOp;

impl AsyncCallOp {
    pub fn new(ctx: &mut Context, callee: Identifier, args: Vec<Value>, value: TypeHandle) -> Self {
        let promise = PromiseType::get(ctx, value).into();
        let op = AsyncCallOp {
            op: Self::new_op(ctx, vec![promise], args),
        };
        op.set_attr_perry_async_callee(ctx, IdentifierAttr::new(callee));
        op
    }

    pub fn callee(&self, ctx: &Context) -> Identifier {
        self.get_attr_perry_async_callee(ctx)
            .expect("callee")
            .clone()
            .into()
    }
}

impl Verify for AsyncCallOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation();
        let callee = self.callee(ctx);
        let (args, results) = callee_signature(ctx, op, &callee)?;
        let promise =
            TypedHandle::<PromiseType>::from_handle(self.get_result(ctx).get_type(ctx), ctx);
        let matches = promise.is_ok_and(|p| results == [p.deref(ctx).value()]);
        if args != operand_types(ctx, op) || !matches {
            verify_err!(self.loc(ctx), CallErr::Signature(callee.to_string()))?
        }
        Ok(())
    }
}

/// Wait for a promise. Before async split (P3.8) this is an ordinary op; the split turns each
/// `await` into a state-machine suspension point. Awaiting a settled promise continues inline.
#[pliron_op(
    name = "perry.await",
    format = "$0 ` : ` type($0)",
    interfaces = [OneResultInterface]
)]
pub struct AwaitOp;

impl AwaitOp {
    pub fn new(ctx: &mut Context, promise: Value) -> Result<Self> {
        let value = TypedHandle::<PromiseType>::from_handle(promise.get_type(ctx), ctx)?
            .deref(ctx)
            .value();
        Ok(AwaitOp {
            op: Self::new_op(ctx, vec![value], vec![promise]),
        })
    }
}

impl Verify for AwaitOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if op.get_num_operands() != 1 {
            verify_err!(self.loc(ctx), CallErr::Await)?
        }
        let promise = TypedHandle::<PromiseType>::from_handle(op.get_operand(0).get_type(ctx), ctx);
        if !promise.is_ok_and(|p| p.deref(ctx).value() == self.get_result(ctx).get_type(ctx)) {
            verify_err!(self.loc(ctx), CallErr::Await)?
        }
        Ok(())
    }
}

side_effects!(true; AwaitOp);

#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for CallOp {
    fn exception(&self, _ctx: &Context) -> crate::attrs::ErrorKind {
        crate::attrs::ErrorKind::Any
    }
}

#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for AwaitOp {
    fn exception(&self, _ctx: &Context) -> crate::attrs::ErrorKind {
        crate::attrs::ErrorKind::Any
    }
}

pliron::dict_key!(FUNCTION_META, "perry_function_meta");

fn update_meta(
    ctx: &mut Context,
    function: &FuncOp,
    update: impl FnOnce(&mut crate::attrs::FunctionMeta),
) {
    let mut meta = function_meta(ctx, function);
    update(&mut meta);
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(FUNCTION_META.clone(), meta);
}

/// Read the function's [crate::attrs::FunctionMeta] flags.
pub(crate) fn function_meta(ctx: &Context, function: &FuncOp) -> crate::attrs::FunctionMeta {
    function
        .get_operation()
        .deref(ctx)
        .attributes
        .get::<crate::attrs::FunctionMeta>(&FUNCTION_META)
        .copied()
        .unwrap_or_default()
}

/// Mark `function` as exported from the component; each call of one is a task (D3, D5).
pub fn set_exported(ctx: &mut Context, function: &FuncOp) {
    update_meta(ctx, function, |m| m.exported = true);
}

/// Whether `function` is exported from the component.
pub fn is_exported(ctx: &Context, function: &FuncOp) -> bool {
    function_meta(ctx, function).exported
}

/// Mark `function` as a specialized implementation entry created by ProgramFacts.
pub(crate) fn set_specialized_entry(ctx: &mut Context, function: &FuncOp) {
    update_meta(ctx, function, |m| m.specialized_entry = true);
}

/// Whether `function` is a specialized implementation entry.
pub(crate) fn is_specialized_entry(ctx: &Context, function: &FuncOp) -> bool {
    function_meta(ctx, function).specialized_entry
}

/// Request one uninterrupted construction scope. ProgramFacts verifies the closed
/// synchronous call group and each operation before codegen omits internal safe points.
pub fn set_atomic_construction(ctx: &mut Context, function: &FuncOp) {
    update_meta(ctx, function, |m| m.atomic_construction = true);
}

/// Whether `function` requested an uninterrupted construction scope.
pub(crate) fn is_atomic_construction(ctx: &Context, function: &FuncOp) -> bool {
    function_meta(ctx, function).atomic_construction
}

/// Mark `function` as an `async` function. Its body may `perry.await`; callers start it
/// with `perry.async_call`, and codegen splits it into a ramp and a state machine (P3.8).
pub fn set_async(ctx: &mut Context, function: &FuncOp) {
    update_meta(ctx, function, |m| m.asynchronous = true);
}

pub fn is_async(ctx: &Context, function: &FuncOp) -> bool {
    function_meta(ctx, function).asynchronous
}

pliron::dict_key!(JSON_DECODER_KIND, "perry_json_decoder_kind");

/// Mark `function` as a compiler-generated JSON decoder with semantic `kind`.
pub fn set_json_decoder(ctx: &mut Context, function: &FuncOp, kind: crate::attrs::JsonDecoderKind) {
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(JSON_DECODER_KIND.clone(), kind);
}

/// The semantic kind of a compiler-generated JSON decoder, if `function` is one.
pub(crate) fn json_decoder_kind(
    ctx: &Context,
    function: &FuncOp,
) -> Option<crate::attrs::JsonDecoderKind> {
    function
        .get_operation()
        .deref(ctx)
        .attributes
        .get::<crate::attrs::JsonDecoderKind>(&JSON_DECODER_KIND)
        .copied()
}

#[derive(Debug, Error)]
#[error("perry.timer takes an f64 delay in milliseconds and settles a perry.promise of its value")]
pub(crate) struct TimerErr;

/// `setTimeout(delay, value, { signal })` from `node:timers/promises`: a promise that settles
/// with `value` once `delay` milliseconds (at least 1, as in Node) have passed on the host's
/// monotonic clock. An optional third operand is the `AbortSignal` that rejects it early.
#[pliron_op(
    name = "perry.timer",
    format = "operands(CharSpace(`,`)) ` : ` type($0)",
    interfaces = [OneResultInterface]
)]
pub struct TimerOp;

impl TimerOp {
    pub fn new(ctx: &mut Context, delay: Value, value: Value, signal: Option<Value>) -> Self {
        let promise = PromiseType::get(ctx, value.get_type(ctx)).into();
        let mut operands = vec![delay, value];
        operands.extend(signal);
        TimerOp {
            op: Self::new_op(ctx, vec![promise], operands),
        }
    }
}

impl Verify for TimerOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if !(2..=3).contains(&op.get_num_operands()) {
            verify_err!(self.loc(ctx), TimerErr)?
        }
        if op.get_num_operands() == 3
            && !op
                .get_operand(2)
                .get_type(ctx)
                .deref(ctx)
                .is::<crate::types::AbortType>()
        {
            verify_err!(self.loc(ctx), TimerErr)?
        }
        let delay = op.get_operand(0).get_type(ctx);
        let value = op.get_operand(1).get_type(ctx);
        let promise =
            TypedHandle::<PromiseType>::from_handle(self.get_result(ctx).get_type(ctx), ctx);
        if !delay.deref(ctx).is::<pliron::builtin::types::FP64Type>()
            || !promise.is_ok_and(|p| p.deref(ctx).value() == value)
        {
            verify_err!(self.loc(ctx), TimerErr)?
        }
        Ok(())
    }
}
