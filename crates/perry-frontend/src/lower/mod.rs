//! Eager MIR construction for specialized TypeScript functions.
mod aggregates;
mod array_methods;
mod bindings;
mod builder;
mod builtins;
mod callable;
mod calls;
mod classes;
mod coerce;
mod collections;
mod control_flow;
mod dates;
mod expressions;
mod foreign;
mod functions;
use functions::{Callee, Functions};
mod host;
mod inline;
mod json;
mod methods;
mod operators;
pub(super) mod program;
mod promises;
mod resources;
mod shapes;
use crate::hir_compat;
use crate::source::Arguments;
mod spread;
mod statements;
mod temporal;
mod timers;
mod url;
mod value_unions;
mod variants;
use perry_hir::{
    ir::{BinaryOp, CompareOp, Expr, LogicalOp, Stmt, UnaryOp, UpdateOp},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_mir::{
    AllocOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp, ArrayTruncateOp,
    ArrayType, AsyncCallOp, AwaitOp, BigIntType, BinaryKind, BinaryOp as MirBinary, BrOp,
    BytesType, CallOp, CatchOp, CaughtOp, Combinator, CompareKind, CompareOp as MirCompare,
    ConcatOp, CondBrOp, ErrorKind, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExceptionType,
    ExitOp, FieldGetOp, FieldPresentOp, FieldSetOp, Intrinsic, IsNullOp, IsUndefinedOp,
    JsonStringifyOp, JsonType, LoadOp, NullOp, NumericCastOp, OmittedOp, OptionalCastOp,
    PlironResultExt as _, PromiseCombinatorOp, PromisePendingOp, PromiseRejectedOp,
    PromiseResolvedOp, PromiseSettleOp, PromiseType, RaiseOp, RecordKind, RecordType, RethrowOp,
    ReturnOp, SettledField, SettledFieldOp, StoreOp, StringLenOp, StringType, TimerOp, ToNumberOp,
    ToStringOp, UndefOp, UndefinedOp, UnwrapOp, WrapOp, absence, admits_undefined, may_throw,
    optional_parts, optional_value, present_type, set_async,
};
use perry_wit_diagnostics::{
    Diagnostic, Result, bail_diag, bail_internal, bail_unsupported, codes, diag, ensure_diag,
    ensure_internal, ensure_unsupported, internal, unsupported,
};
use pliron::printable::Printable as _;
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::IntegerAttr,
        op_interfaces::{OneResultInterface, SingleBlockRegionInterface},
        ops::{ConstantOp, FuncOp, ModuleOp},
        types::{FP64Type, FunctionType, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    identifier::{Identifier, Legaliser},
    irbuild::{
        inserter::{IRInserter, Inserter},
        listener::DummyListener,
    },
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
    value::Value,
};
use shapes::{BindingType, Shapes, identifier};
use std::collections::{HashMap, HashSet};

use builder::{Control, EXIT_STATUS, Finally, FunctionBuilder, Handler, LocalBinding};
use coerce::{Kind, is_integer64, kind, void_type};
use program::{Builtin, Signature};

/// The function `wasi:cli/run` binds to: the top-level statements of a module without exported
/// functions, otherwise an entry point awaiting module initialization.
pub const MAIN: &str = "perry_main";

fn missing_type_annotation() -> Diagnostic {
    Diagnostic::new(
        codes::MISSING_TYPE_ANNOTATION,
        "the value type cannot be inferred",
    )
    .with_help("Add a concrete type annotation to this binding, parameter, or function return.")
}

pub enum ExportTarget {
    Destructor(FuncOp, String),
    Function(
        FuncOp,
        Vec<Option<perry_wit_diagnostics::SourceSpan>>,
        Option<String>,
    ),
    Command(FuncOp),
}

pub struct LoweredProgram {
    pub module: ModuleOp,
    pub exports: Vec<ExportTarget>,
}

#[cfg(test)]
mod tests;

use crate::engine::BodyOwner;
use crate::source::InstanceFunction as Function;
