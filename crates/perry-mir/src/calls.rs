//! Direct calls and async calls to functions of the enclosing module.
//!
//! Perry has no first-class functions in MIR (subset rule R5): every callee is a symbol,
//! which keeps escape summaries and inlining (P3.2, P3.3) exact.
use crate::operation_schema::op_schema;
use crate::signatures::{OutputContract, ResultType, Signature, Ty};
use crate::types::PromiseType;
use pliron::{
    builtin::{
        attributes::IdentifierAttr, op_interfaces::SymbolTableInterface, ops::FuncOp,
        type_interfaces::FunctionTypeInterface, types::FunctionType,
    },
    common_traits::Verify,
    context::{Context, Ptr},
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

op_schema! {
    /// Synchronous call; results are the callee's results.
    pub CallOp(.._operands) -> infallible;
    name = "perry.call";
    format = "attr($perry_callee, $IdentifierAttr) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))";
    attributes = (perry_callee: IdentifierAttr);
    constructors {
        pub new(callee: Identifier, args: Vec<Value>, results: Vec<TypeHandle>)
            operands(args) => results(results);
    }
    attribute(callee, get_attr_perry_callee, set_attr_perry_callee) = IdentifierAttr::new(callee)
        => CallErr::Unknown("<missing>".into());
    prepare(this) {
        let callee = callee.as_ref();
        let (args, results) = callee_signature(ctx, this.get_operation(), callee)?;
    }
    result(ctx, args: Vec<TypeHandle>, results: Vec<TypeHandle>): impl OutputContract =
        Signature { inputs: args, outputs: results };
    error = CallErr::Signature(callee.to_string());
}

impl CallOp {
    pub fn callee(&self, ctx: &Context) -> Identifier {
        self.get_attr_perry_callee(ctx)
            .expect("callee")
            .clone()
            .into()
    }
}

op_schema! {
    /// Start an async function eagerly; it runs until its first pending `await` (D5).
    pub AsyncCallOp(.._operands) -> infallible;
    name = "perry.async_call";
    format = "attr($perry_async_callee, $IdentifierAttr) ` (` operands(CharSpace(`,`)) `) : ` type($0)";
    attributes = (perry_async_callee: IdentifierAttr);
    constructors {
        pub new(callee: Identifier, args: Vec<Value>, value: TypeHandle)
            operands(args) => explicit(PromiseType::get(ctx, value).into());
    }
    attribute(callee, get_attr_perry_async_callee, set_attr_perry_async_callee) = IdentifierAttr::new(callee)
        => CallErr::Unknown("<missing>".into());
    prepare(this) {
        let callee = callee.as_ref();
        let (args, results) = callee_signature(ctx, this.get_operation(), callee)?;
    }
    result(ctx, args: Vec<TypeHandle>, results: Vec<TypeHandle>): impl OutputContract =
        Signature {
            inputs: args,
            outputs: results.into_iter().map(|ty| TypeHandle::from(PromiseType::get(ctx, ty))),
        };
    error = CallErr::Signature(callee.to_string());
    check_before results.len() == 1 => CallErr::Signature(callee.to_string());
}

impl AsyncCallOp {
    pub fn callee(&self, ctx: &Context) -> Identifier {
        self.get_attr_perry_async_callee(ctx)
            .expect("callee")
            .clone()
            .into()
    }
}

op_schema! {
    /// Wait for a promise. Before async split (P3.8) this is an ordinary op; the split turns each
    /// `await` into a state-machine suspension point. Awaiting a settled promise continues inline.
    pub AwaitOp(promise) -> fallible;
    name = "perry.await";
    format = "$0 ` : ` type($0)";
    constructors { pub new(promise: Value) => infer(promise); }
    result(ctx, promise: Value): TypeHandle = Ok(
        TypedHandle::<PromiseType>::from_handle(promise.get_type(ctx), ctx)?
            .deref(ctx)
            .value(),
    );
    error = CallErr::Await;
    invalid_source = CallErr::Await;
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

#[derive(Debug, Error)]
#[error("perry.timer takes an f64 delay in milliseconds and settles a perry.promise of its value")]
pub(crate) struct TimerErr;

op_schema! {
    /// `setTimeout(delay, value, { signal })` from `node:timers/promises`: a promise that settles
    /// with `value` once `delay` milliseconds (at least 1, as in Node) have passed on the host's
    /// monotonic clock. An optional third operand is the `AbortSignal` that rejects it early.
    pub TimerOp(..operands) -> infallible;
    name = "perry.timer";
    format = "operands(CharSpace(`,`)) ` : ` type($0)";
    constructors {
        pub new(delay: Value, value: Value, signal: Option<Value>) operands({
            let mut args = vec![delay, value];
            args.extend(signal);
            args
        }) => infer(value.get_type(ctx));
    }
    prepare(this) {
        if !(2..=3).contains(&operands.get_num_operands()) {
            return verify_err!(this.loc(ctx), TimerErr);
        }
        let value = operands.get_operand(1).get_type(ctx);
    }
    result(ctx, value: TypeHandle): TypeHandle = PromiseType::get(ctx, value).into();
    error = TimerErr;
    check_before Ty::Number.matches(ctx, operands.get_operand(0).get_type(ctx))
        && (operands.get_num_operands() == 2 || Ty::Abort.matches(ctx, operands.get_operand(2).get_type(ctx)))
        => TimerErr;
}
