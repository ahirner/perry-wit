//! Exception model.
//!
//! Runtime failures in the supported subset are ECMAScript exceptions that `try`/`catch` can
//! observe. Wasm traps are reserved for resource exhaustion and compiler bugs because a trap
//! aborts the component instance.
//!
//! An op with an exception contract may throw. Exceptions travel as an `i32` status
//! (`0` for normal completion or an [ErrorKind] code) together with a pending exception
//! object held by the task for `throw` statements. Outside a `try`, a throwing op leaves the
//! function with its status. Inside a `try`, the builder follows it with [CaughtOp], which
//! yields the status of the preceding op, suppresses the implicit exit, and branches to the
//! handler when the status is non-zero.
//!
//! - [RaiseOp] records a thrown value as the pending exception and yields its status, and
//!   [RethrowOp] leaves the function with a status.
//! - [CatchOp] takes the pending exception or materializes one for a status raised by the
//!   runtime, such as the `TypeError` from `perry.unwrap`.
//! - [ErrorNewOp], [ExceptionIsOp], and [ExceptionMessageOp] implement `new Error(...)`,
//!   `instanceof`, and `.message`.
use crate::attrs::ErrorKind;
use crate::operation_schema::op_schema;
use crate::signatures::{ResultType, Ty};
use pliron::{
    builtin::op_interfaces::IsTerminatorInterface, common_traits::Verify, context::Context,
    linked_list::LinkedList, op::Op, operation::Operation, result::Result, r#type::Typed,
    value::Value,
};

#[derive(Debug)]
enum ExceptionErr {
    Exception(&'static str),
    Status(&'static str),
    Caught,
    Message,
    Result(&'static str),
}

impl core::fmt::Display for ExceptionErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exception(op) => write!(f, "{op} expects a perry.exception operand"),
            Self::Status(op) => write!(f, "{op} expects a signless i32 status"),
            Self::Caught => f.write_str("perry.caught must directly follow an op that may throw"),
            Self::Message => f.write_str("perry.error_new takes a perry.string message"),
            Self::Result(op) => write!(f, "{op} has a wrong result type"),
        }
    }
}

impl core::error::Error for ExceptionErr {}

op_schema! {
    /// `new Error(message)`, `new TypeError(message)`, `new RangeError(message)` (kind `any` is
    /// plain `Error`).
    pub ErrorNewOp(message) -> infallible;
    name = "perry.error_new";
    format = "attr($perry_error_kind, $ErrorKind) ` ` $0";
    attributes = (perry_error_kind: ErrorKind);
    constructors { pub new(kind: ErrorKind, message: Value) => infer(); }
    attribute(kind: ErrorKind, get_attr_perry_error_kind, set_attr_perry_error_kind) = kind;
    result(ctx): Ty = Ty::Exception;
    error = ExceptionErr::Result("perry.error_new");
    check_before Ty::String.matches(ctx, message.get_type(ctx)) => ExceptionErr::Message;
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
    /// Read the status of the immediately preceding op (`0` on normal completion) and keep
    /// control within the current function when that op throws.
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
    attribute(kind: ErrorKind, get_attr_perry_instance_kind, set_attr_perry_instance_kind) = kind;
    result(ctx): Ty = Ty::Predicate;
    error = ExceptionErr::Result("perry.exception_is");
    check_before Ty::Exception.matches(ctx, exception.get_type(ctx))
        => ExceptionErr::Exception("perry.exception_is");
}

op_schema! {
    /// Read `e.message` from an `Error` instance, returning the empty string for other thrown values.
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
