//! Exception model.
//!
//! Runtime failures in the supported subset are ECMAScript exceptions that `try`/`catch` can
//! observe; Wasm traps are reserved for resource exhaustion and compiler bugs, because a trap
//! aborts the component instance and cannot be caught by guest code.
//!
//! An op with an exception contract may throw. Exceptions travel as an `i32` status
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
use crate::attrs::ErrorKind;
use crate::operation_schema::op_schema;
use crate::signatures::{ResultType, Ty};
use pliron::{
    builtin::op_interfaces::IsTerminatorInterface, common_traits::Verify, context::Context,
    linked_list::LinkedList, op::Op, operation::Operation, result::Result, r#type::Typed,
    value::Value,
};
use thiserror::Error;

#[derive(Debug, Error)]
enum ExceptionErr {
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

op_schema! {
    /// `new Error(message)`, `new TypeError(message)`, `new RangeError(message)` (kind `any` is
    /// plain `Error`).
    pub ErrorNewOp(message) -> infallible;
    name = "perry.error_new";
    format = "attr($perry_error_kind, $ErrorKind) ` ` $0";
    attributes = (perry_error_kind: ErrorKind);
    constructors { pub new(kind: ErrorKind, message: Value) => infer(); }
    initialize(op) {
        op.set_attr_perry_error_kind(ctx, kind);
    }
    result(ctx): Ty = Ty::Exception;
    error = ExceptionErr::Result("perry.error_new");
    check_before Ty::String.matches(ctx, message.get_type(ctx)) => ExceptionErr::Message;
}

impl ErrorNewOp {
    pub fn kind(&self, ctx: &Context) -> ErrorKind {
        *self.get_attr_perry_error_kind(ctx).expect("error kind")
    }
}

op_schema! {
    /// `throw value`: make `value` the pending exception and yield its status. The value is a
    /// `perry.exception`, a number, a boolean or a string.
    pub RaiseOp(_value) -> infallible;
    name = "perry.raise";
    format = "$0 ` : ` type($0)";
    constructors { pub new(_value: Value) => infer(); }
    result(ctx): Ty = Ty::Status;
    error = ExceptionErr::Result("perry.raise");
}

op_schema! {
    /// `process.exit(code)`: record the exit code and yield the [ErrorKind::Exit] status, which
    /// handlers pass on without running.
    pub ExitOp(_code) -> infallible;
    name = "perry.exit";
    format = "$0";
    constructors { pub new(_code: Value) => infer(); }
    result(ctx): Ty = Ty::Status;
    error = ExceptionErr::Result("perry.exit");
}

op_schema! {
    /// Leave the function with exception status `status` (non-zero).
    pub RethrowOp(status) -> infallible;
    name = "perry.rethrow";
    format = "$0";
    interfaces = [IsTerminatorInterface];
    constructors { pub new(status: Value) => empty(); }
    result(ctx): () = ();
    error = ExceptionErr::Status("perry.rethrow");
    check Ty::Status.matches(ctx, status.get_type(ctx)) => ExceptionErr::Status("perry.rethrow");
}

op_schema! {
    /// The status of the op just before it (0 if it completed normally); that op then does not
    /// leave the function when it throws.
    pub CaughtOp() -> infallible;
    name = "perry.caught";
    format = "` : ` type($0)";
    constructors { pub new() => infer(); }
    prepare(this) {
        let previous = this.get_operation().deref(ctx).get_prev();
        let throws = previous.is_some_and(|previous| {
            crate::semantics::exception(ctx, previous).is_some()
        });
    }
    result(ctx): Ty = Ty::Status;
    error = ExceptionErr::Result("perry.caught");
    check_before throws => ExceptionErr::Caught;
}

op_schema! {
    /// The exception for `status`: the pending one, or a fresh `Error` of the status' kind for
    /// exceptions raised by the runtime. Clears the pending exception.
    pub CatchOp(status) -> infallible;
    name = "perry.catch";
    format = "$0";
    constructors { pub new(status: Value) => infer(); }
    result(ctx): Ty = Ty::Exception;
    error = ExceptionErr::Result("perry.catch");
    check_before Ty::Status.matches(ctx, status.get_type(ctx)) => ExceptionErr::Status("perry.catch");
}

op_schema! {
    /// `e instanceof Error` (kind `any`), `TypeError` or `RangeError`.
    pub ExceptionIsOp(exception) -> infallible;
    name = "perry.exception_is";
    format = "attr($perry_instance_kind, $ErrorKind) ` ` $0";
    attributes = (perry_instance_kind: ErrorKind);
    constructors { pub new(kind: ErrorKind, exception: Value) => infer(); }
    initialize(op) {
        op.set_attr_perry_instance_kind(ctx, kind);
    }
    result(ctx): Ty = Ty::Predicate;
    error = ExceptionErr::Result("perry.exception_is");
    check_before Ty::Exception.matches(ctx, exception.get_type(ctx))
        => ExceptionErr::Exception("perry.exception_is");
}

impl ExceptionIsOp {
    pub fn kind(&self, ctx: &Context) -> ErrorKind {
        *self.get_attr_perry_instance_kind(ctx).expect("error kind")
    }
}

op_schema! {
    /// `e.message` of an `Error`; the empty string for other thrown values.
    pub ExceptionMessageOp(exception) -> infallible;
    name = "perry.exception_message";
    format = "$0";
    constructors { pub new(exception: Value) => infer(); }
    result(ctx): Ty = Ty::String;
    error = ExceptionErr::Result("perry.exception_message");
    check_before Ty::Exception.matches(ctx, exception.get_type(ctx)) => ExceptionErr::Exception("perry.exception_message");
}

/// Mark the status-only bypass that sends process.exit around catch and finally.
pub fn set_exit_only(ctx: &mut Context, op: &RethrowOp) {
    op.get_operation().deref_mut(ctx).attributes.set(
        pliron::identifier::Identifier::new("perry_exit_only"),
        pliron::builtin::attributes::BoolAttr::from(true),
    );
}

/// Local handling suppresses an export error channel, while retaining the operation's status.
pub(crate) fn is_handled(ctx: &Context, op: pliron::context::Ptr<Operation>) -> bool {
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
