//! Exception model.
//!
//! Runtime failures in the supported subset are ECMAScript exceptions that `try`/`catch` can
//! observe; Wasm traps are reserved for resource exhaustion and compiler bugs, because a trap
//! aborts the component instance and cannot be caught by guest code.
//!
//! An op implementing [MayThrowInterface] may throw. Inside a handler, [CaughtStatusOp]
//! immediately follows it and supplies the status for an explicit normal/exceptional branch.
//! Unhandled failures leave through the function's exceptional exit. Codegen uses an extra
//! status result and a task-local pending value; MIR exception values remain abstract.
use crate::attrs::ErrorKind;
use pliron::{context::Context, derive::op_interface, op::Op, result::Result};

#[op_interface]
pub trait MayThrowInterface {
    /// The error this op raises when it throws.
    fn exception(&self, ctx: &Context) -> ErrorKind;

    /// Whether this particular operation can fail (for table-driven intrinsics).
    fn may_throw(&self, _ctx: &Context) -> bool {
        true
    }

    fn verify(_op: &dyn Op, _ctx: &Context) -> Result<()>
    where
        Self: Sized,
    {
        Ok(())
    }
}

use crate::{
    scalar::is_bool,
    types::{ExceptionType, OptionalType, StringType},
};
use pliron::{
    builtin::{
        op_interfaces::{
            IsTerminatorInterface, NOpdsInterface, NResultsInterface, OneOpdInterface,
            OneResultInterface,
        },
        types::{FP64Type, IntegerType, Signedness},
    },
    common_traits::Verify,
    derive::{op_interface_impl, pliron_op},
    linked_list::LinkedList,
    op::op_cast,
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ExceptionErr(&'static str);

fn is_status(ctx: &Context, ty: TypeHandle) -> bool {
    ty == IntegerType::get(ctx, 32, Signedness::Signless).into()
}

/// Immediately follows a MayThrow op. Its status is zero on normal completion and nonzero
/// on failure; only the zero branch may use the preceding op's results. It is effectful so
/// canonicalization keeps the pair together even if the status or normal result is unused.
#[pliron_op(name = "perry.caught_status", format = "` : ` type($0)", interfaces = [NOpdsInterface<0>, OneResultInterface])]
pub struct CaughtStatusOp;

impl CaughtStatusOp {
    pub fn new(ctx: &mut Context) -> Self {
        let ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for CaughtStatusOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let previous = self.op.deref(ctx).get_prev();
        if !is_status(ctx, self.get_result(ctx).get_type(ctx))
            || !previous.is_some_and(|op| {
                op_cast::<dyn MayThrowInterface>(Operation::get_op_dyn(op, ctx).as_ref())
                    .is_some_and(|op| op.may_throw(ctx))
                    && Operation::get_op::<RethrowOp>(op, ctx).is_none()
            })
        {
            verify_err!(
                self.loc(ctx),
                ExceptionErr(
                    "caught_status must immediately follow a throwing operation and produce i32"
                )
            )?;
        }
        Ok(())
    }
}

macro_rules! unary {
    ($name:ident, $opname:literal, $result:expr) => {
        #[pliron_op(name = $opname, format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
        pub struct $name;
        impl $name {
            pub fn new(ctx: &mut Context, value: Value) -> Self {
                let ty: TypeHandle = ($result)(ctx);
                Self { op: Operation::new(ctx, Self::get_concrete_op_info(), vec![ty], vec![value], vec![], 0) }
            }
        }
    };
}

unary!(BoxExceptionOp, "perry.box_exception", |ctx: &Context| {
    ExceptionType::get(ctx).into()
});
unary!(RaiseOp, "perry.raise", |ctx: &Context| IntegerType::get(
    ctx,
    32,
    Signedness::Signless
)
.into());
unary!(CatchOp, "perry.catch", |ctx: &Context| ExceptionType::get(
    ctx
)
.into());
unary!(
    ExceptionMessageOp,
    "perry.exception_message",
    |ctx: &Context| {
        let string = StringType::get(ctx).into();
        OptionalType::get(ctx, string).into()
    }
);

impl Verify for BoxExceptionOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let value = self.op.deref(ctx).get_operand(0);
        let ty = value.get_type(ctx);
        if !(ty.deref(ctx).is::<FP64Type>()
            || ty.deref(ctx).is::<StringType>()
            || ty.deref(ctx).is::<ExceptionType>()
            || is_bool(ctx, value))
            || !self
                .get_result(ctx)
                .get_type(ctx)
                .deref(ctx)
                .is::<ExceptionType>()
        {
            verify_err!(
                self.loc(ctx),
                ExceptionErr(
                    "box_exception needs an Error, number, boolean or string and produces an exception"
                )
            )?;
        }
        Ok(())
    }
}

impl Verify for RaiseOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !self
            .op
            .deref(ctx)
            .get_operand(0)
            .get_type(ctx)
            .deref(ctx)
            .is::<ExceptionType>()
            || !is_status(ctx, self.get_result(ctx).get_type(ctx))
        {
            verify_err!(
                self.loc(ctx),
                ExceptionErr("raise needs an exception and produces a status")
            )?;
        }
        Ok(())
    }
}

impl Verify for CatchOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_status(ctx, self.op.deref(ctx).get_operand(0).get_type(ctx))
            || !self
                .get_result(ctx)
                .get_type(ctx)
                .deref(ctx)
                .is::<ExceptionType>()
        {
            verify_err!(
                self.loc(ctx),
                ExceptionErr("catch needs a status and produces an exception")
            )?;
        }
        Ok(())
    }
}

impl Verify for ExceptionMessageOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !self
            .op
            .deref(ctx)
            .get_operand(0)
            .get_type(ctx)
            .deref(ctx)
            .is::<ExceptionType>()
            || crate::optional::optional_value(ctx, self.get_result(ctx).get_type(ctx))
                != Some(StringType::get(ctx).into())
        {
            verify_err!(
                self.loc(ctx),
                ExceptionErr(
                    "exception_message needs an exception and produces string | undefined"
                )
            )?;
        }
        Ok(())
    }
}

/// Create a distinct Error object with its message.
#[pliron_op(name = "perry.error_new", format = "attr($perry_new_error_kind, $ErrorKind) ` ` $0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface], attributes = (perry_new_error_kind: ErrorKind))]
pub struct ErrorNewOp;
impl ErrorNewOp {
    pub fn new(ctx: &mut Context, kind: ErrorKind, message: Value) -> Self {
        let ty = ExceptionType::get(ctx).into();
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![message],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_new_error_kind(ctx, kind);
        op
    }
    pub fn kind(&self, ctx: &Context) -> ErrorKind {
        *self.get_attr_perry_new_error_kind(ctx).expect("error kind")
    }
}
impl Verify for ErrorNewOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !self
            .op
            .deref(ctx)
            .get_operand(0)
            .get_type(ctx)
            .deref(ctx)
            .is::<StringType>()
            || !self
                .get_result(ctx)
                .get_type(ctx)
                .deref(ctx)
                .is::<ExceptionType>()
        {
            verify_err!(
                self.loc(ctx),
                ExceptionErr("error_new needs a string message and produces an exception")
            )?;
        }
        Ok(())
    }
}

/// `instanceof` on a caught value. `Any` denotes the Error base class here.
#[pliron_op(name = "perry.exception_is", format = "attr($perry_test_error_kind, $ErrorKind) ` ` $0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface], attributes = (perry_test_error_kind: ErrorKind))]
pub struct ExceptionIsOp;
impl ExceptionIsOp {
    pub fn new(ctx: &mut Context, kind: ErrorKind, value: Value) -> Self {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![value],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_test_error_kind(ctx, kind);
        op
    }
    pub fn kind(&self, ctx: &Context) -> ErrorKind {
        *self
            .get_attr_perry_test_error_kind(ctx)
            .expect("error kind")
    }
}
impl Verify for ExceptionIsOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !self
            .op
            .deref(ctx)
            .get_operand(0)
            .get_type(ctx)
            .deref(ctx)
            .is::<ExceptionType>()
            || !is_bool(ctx, self.get_result(ctx))
        {
            verify_err!(
                self.loc(ctx),
                ExceptionErr("exception_is needs an exception and produces a boolean")
            )?;
        }
        Ok(())
    }
}

/// Propagate the pending exception out of the function.
#[pliron_op(name = "perry.rethrow", format = "$0", interfaces = [OneOpdInterface, NResultsInterface<0>, IsTerminatorInterface])]
pub struct RethrowOp;
impl RethrowOp {
    pub fn new(ctx: &mut Context, status: Value) -> Self {
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![status],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for RethrowOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_status(ctx, self.op.deref(ctx).get_operand(0).get_type(ctx)) {
            verify_err!(self.loc(ctx), ExceptionErr("rethrow needs an i32 status"))?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl MayThrowInterface for RethrowOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::Any
    }
}
