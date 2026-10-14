//! Exception model.
//!
//! Runtime failures in the supported subset are ECMAScript exceptions that `try`/`catch` can
//! observe; Wasm traps are reserved for resource exhaustion and compiler bugs, because a trap
//! aborts the component instance and cannot be caught by guest code.
//!
//! An op implementing [MayThrowInterface] may throw. Exceptions travel as an `i32` status
//! (0 = none, otherwise an [ErrorKind] code) plus, for `throw` statements, a pending exception
//! object held by the task. Outside a `try`, a throwing op leaves the function with its status.
//! Inside a `try`, the builder follows it with [CaughtOp], which yields the status of the op
//! just before it and suppresses that implicit exit, then branches to the handler on non-zero.
//!
//! - [RaiseOp] records a thrown value as the pending exception and yields its status;
//!   [RethrowOp] leaves the function with a status.
//! - [CatchOp] takes the pending exception (or materializes one for a status raised by the
//!   runtime, such as the `TypeError` of `perry.unwrap`).
//! - [ErrorNewOp], [ExceptionIsOp] and [ExceptionMessageOp] cover `new Error(...)`,
//!   `instanceof` and `.message`.
use crate::{
    attrs::ErrorKind,
    scalar::is_bool,
    types::{ExceptionType, StringType},
};
use pliron::{
    builtin::{
        op_interfaces::{
            IsTerminatorInterface, NOpdsInterface, NResultsInterface, OneOpdInterface,
            OneResultInterface,
        },
        types::{IntegerType, Signedness},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface, op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::Typed,
    value::Value,
    verify_err,
};
use thiserror::Error;

#[op_interface]
pub trait MayThrowInterface {
    /// The error this op raises when it throws.
    fn exception(&self, ctx: &Context) -> ErrorKind;

    fn verify(_op: &dyn Op, _ctx: &Context) -> Result<()>
    where
        Self: Sized,
    {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum ExceptionErr {
    #[error("{0} expects a perry.exception operand")]
    Exception(&'static str),
    #[error("{0} expects a signless i32 status")]
    Status(&'static str),
    #[error("perry.caught must directly follow an op that may throw")]
    Caught,
    #[error("perry.error_new takes a perry.string message")]
    Message,
    #[error("{0} has a wrong result type")]
    Result(&'static str),
}

fn is_i32(ctx: &Context, value: Value) -> bool {
    value
        .get_type(ctx)
        .deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|ty| ty.width() == 32 && ty.signedness() == Signedness::Signless)
}

fn is_exception(ctx: &Context, value: Value) -> bool {
    value.get_type(ctx).deref(ctx).is::<ExceptionType>()
}

fn i32_type(ctx: &mut Context) -> pliron::r#type::TypeHandle {
    IntegerType::get(ctx, 32, Signedness::Signless).into()
}

macro_rules! effects {
    ($value:expr; $($op:ty),*) => {$(
        #[op_interface_impl]
        impl SideEffects for $op {
            fn has_side_effects(&self, _ctx: &Context) -> bool {
                $value
            }
        }
    )*};
}

/// `new Error(message)`, `new TypeError(message)`, `new RangeError(message)` (kind `any` is
/// plain `Error`).
#[pliron_op(
    name = "perry.error_new",
    format = "attr($perry_error_kind, $ErrorKind) ` ` $0",
    interfaces = [OneOpdInterface, OneResultInterface],
    attributes = (perry_error_kind: ErrorKind)
)]
pub struct ErrorNewOp;

impl ErrorNewOp {
    pub fn new(ctx: &mut Context, kind: ErrorKind, message: Value) -> Self {
        let ty = ExceptionType::get(ctx).into();
        let op = ErrorNewOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![message],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_error_kind(ctx, kind);
        op
    }

    pub fn kind(&self, ctx: &Context) -> ErrorKind {
        *self.get_attr_perry_error_kind(ctx).expect("error kind")
    }
}

impl Verify for ErrorNewOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let message = self.get_operation().deref(ctx).get_operand(0);
        if !message.get_type(ctx).deref(ctx).is::<StringType>() {
            verify_err!(self.loc(ctx), ExceptionErr::Message)?
        }
        if !is_exception(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), ExceptionErr::Result("perry.error_new"))?
        }
        Ok(())
    }
}

/// `throw value`: make `value` the pending exception and yield its status. The value is a
/// `perry.exception`, a number, a boolean or a string.
#[pliron_op(
    name = "perry.raise",
    format = "$0 ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct RaiseOp;

impl RaiseOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let i32 = i32_type(ctx);
        RaiseOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![i32],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for RaiseOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_i32(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), ExceptionErr::Result("perry.raise"))?
        }
        Ok(())
    }
}

/// `process.exit(code)`: record the exit code and yield the [ErrorKind::Exit] status, which
/// handlers pass on without running.
#[pliron_op(
    name = "perry.exit",
    format = "$0",
    interfaces = [OneOpdInterface, OneResultInterface],
    verifier = "succ"
)]
pub struct ExitOp;

impl ExitOp {
    pub fn new(ctx: &mut Context, code: Value) -> Self {
        let i32 = i32_type(ctx);
        ExitOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![i32],
                vec![code],
                vec![],
                0,
            ),
        }
    }
}

/// Leave the function with exception status `status` (non-zero).
#[pliron_op(
    name = "perry.rethrow",
    format = "$0",
    interfaces = [IsTerminatorInterface, NResultsInterface<0>, OneOpdInterface]
)]
pub struct RethrowOp;

impl RethrowOp {
    pub fn new(ctx: &mut Context, status: Value) -> Self {
        RethrowOp {
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
        if !is_i32(ctx, self.get_operation().deref(ctx).get_operand(0)) {
            verify_err!(self.loc(ctx), ExceptionErr::Status("perry.rethrow"))?
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

/// The status of the op just before it (0 if it completed normally); that op then does not
/// leave the function when it throws.
#[pliron_op(
    name = "perry.caught",
    format = "` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface]
)]
pub struct CaughtOp;

impl CaughtOp {
    pub fn new(ctx: &mut Context) -> Self {
        let i32 = i32_type(ctx);
        CaughtOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![i32],
                vec![],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for CaughtOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let previous = {
            use pliron::linked_list::LinkedList;
            self.get_operation().deref(ctx).get_prev()
        };
        let throws = previous.is_some_and(|previous| {
            pliron::op::op_cast::<dyn MayThrowInterface>(
                Operation::get_op_dyn(previous, ctx).as_ref(),
            )
            .is_some()
        });
        if !throws {
            verify_err!(self.loc(ctx), ExceptionErr::Caught)?
        }
        Ok(())
    }
}

/// The exception for `status`: the pending one, or a fresh `Error` of the status' kind for
/// exceptions raised by the runtime. Clears the pending exception.
#[pliron_op(
    name = "perry.catch",
    format = "$0",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct CatchOp;

impl CatchOp {
    pub fn new(ctx: &mut Context, status: Value) -> Self {
        let ty = ExceptionType::get(ctx).into();
        CatchOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![status],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for CatchOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_i32(ctx, self.get_operation().deref(ctx).get_operand(0)) {
            verify_err!(self.loc(ctx), ExceptionErr::Status("perry.catch"))?
        }
        Ok(())
    }
}

/// `e instanceof Error` (kind `any`), `TypeError` or `RangeError`.
#[pliron_op(
    name = "perry.exception_is",
    format = "attr($perry_instance_kind, $ErrorKind) ` ` $0",
    interfaces = [OneOpdInterface, OneResultInterface],
    attributes = (perry_instance_kind: ErrorKind)
)]
pub struct ExceptionIsOp;

impl ExceptionIsOp {
    pub fn new(ctx: &mut Context, kind: ErrorKind, exception: Value) -> Self {
        let i1 = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let op = ExceptionIsOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![i1],
                vec![exception],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_instance_kind(ctx, kind);
        op
    }

    pub fn kind(&self, ctx: &Context) -> ErrorKind {
        *self.get_attr_perry_instance_kind(ctx).expect("error kind")
    }
}

impl Verify for ExceptionIsOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_exception(ctx, self.get_operation().deref(ctx).get_operand(0)) {
            verify_err!(self.loc(ctx), ExceptionErr::Exception("perry.exception_is"))?
        }
        if !is_bool(ctx, self.get_result(ctx)) {
            verify_err!(self.loc(ctx), ExceptionErr::Result("perry.exception_is"))?
        }
        Ok(())
    }
}

/// `e.message` of an `Error`; the empty string for other thrown values.
#[pliron_op(
    name = "perry.exception_message",
    format = "$0",
    interfaces = [OneOpdInterface, OneResultInterface]
)]
pub struct ExceptionMessageOp;

impl ExceptionMessageOp {
    pub fn new(ctx: &mut Context, exception: Value) -> Self {
        let ty = StringType::get(ctx).into();
        ExceptionMessageOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![exception],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for ExceptionMessageOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !is_exception(ctx, self.get_operation().deref(ctx).get_operand(0)) {
            verify_err!(
                self.loc(ctx),
                ExceptionErr::Exception("perry.exception_message")
            )?
        }
        Ok(())
    }
}

effects!(false; ErrorNewOp, ExceptionIsOp, ExceptionMessageOp);
effects!(true; RaiseOp, ExitOp, CaughtOp, CatchOp);

/// Mark the status-only bypass that sends process.exit around catch and finally.
pub fn set_exit_only(ctx: &mut Context, op: &RethrowOp) {
    op.get_operation().deref_mut(ctx).attributes.set(
        pliron::identifier::Identifier::new("perry_exit_only"),
        pliron::builtin::attributes::BoolAttr::from(true),
    );
}

/// Local handling suppresses an export error channel, while retaining the operation's status.
pub(crate) fn is_handled(ctx: &Context, op: pliron::context::Ptr<Operation>) -> bool {
    use pliron::linked_list::LinkedList;
    op.deref(ctx)
        .get_next()
        .is_some_and(|next| Operation::get_op::<CaughtOp>(next, ctx).is_some())
        || Operation::get_op::<RethrowOp>(op, ctx).is_some()
            && op
                .deref(ctx)
                .attributes
                .get::<pliron::builtin::attributes::BoolAttr>(&pliron::identifier::Identifier::new(
                    "perry_exit_only",
                ))
                .is_some_and(|flag| bool::from(flag.clone()))
}
