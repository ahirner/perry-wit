//! Direct calls and async calls to functions of the enclosing module.
//!
//! Perry has no first-class functions in MIR (subset rule R5): every callee is a symbol,
//! which keeps escape summaries and inlining (P3.2, P3.3) exact.
use crate::types::PromiseType;
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
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CallErr {
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
            op: Operation::new(ctx, Self::get_concrete_op_info(), results, args, vec![], 0),
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
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![promise],
                args,
                vec![],
                0,
            ),
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
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![value],
                vec![promise],
                vec![],
                0,
            ),
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

#[op_interface_impl]
impl SideEffects for AwaitOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        true
    }
}
