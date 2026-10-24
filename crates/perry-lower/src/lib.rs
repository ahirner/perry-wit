//! Perry HIR → perry-mir builder.
//!
//! Structural lowering from HIR into MIR follows three principles:
//! - Every local (including parameters) lowers to a `perry.local` slot, and pliron `mem2reg`
//!   constructs SSA joins, loop-carried block arguments, and await continuations.
//! - Contextual inference completes unresolved source types before MIR construction.
//! - Allocation tiers start `Unassigned` and layouts remain abstract for downstream MIR and codegen passes.
//!
//! [lower_sources] resolves and specializes source through `perry_frontend`, then
//! [lower_program] elaborates the frontend program into MIR. Each function body is lowered by a
//! `builder::FunctionBuilder`, whose methods are organized across the topical modules below.
//! Unsupported constructs produce a source-spanned diagnostic.
mod aggregates;
mod array_methods;
mod bindings;
mod builder;
mod builtins;
mod calls;
mod classes;
mod coerce;
mod collections;
mod control_flow;
mod dates;
mod diagnostics;
mod expressions;
mod foreign;
mod host;
mod inline;
mod json;
mod methods;
mod operators;
mod program;
mod shapes;
use perry_frontend::hir_compat;
pub use perry_frontend::source_files;
mod spread;
mod statements;
mod temporal;
mod url;
mod variants;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_frontend::hir::{
    ir::{
        BinaryOp, CompareOp, Expr, Function, LogicalOp, Module as HirModule, Stmt, UnaryOp,
        UpdateOp,
    },
    types::{FuncId, LocalId, Type as HirType},
};
use perry_mir::{
    AllocOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp, ArrayTruncateOp,
    ArrayType, AsyncCallOp, AwaitOp, BigIntType, BinaryKind, BinaryOp as MirBinary, BrOp,
    BytesType, CallOp, CatchOp, CaughtOp, Combinator, CompareKind, CompareOp as MirCompare,
    ConcatOp, CondBrOp, ErrorKind, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExceptionType,
    ExitOp, FieldGetOp, FieldPresentOp, FieldSetOp, Intrinsic, IsNullOp, IsUndefinedOp,
    JsonStringifyOp, JsonType, LoadOp, NullOp, NumericCastOp, OmittedOp, OptionalCastOp,
    PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp, PromiseType, RaiseOp, RecordKind,
    RecordType, RethrowOp, ReturnOp, SettledField, SettledFieldOp, StoreOp, StringLenOp,
    StringType, TimerOp, ToNumberOp, ToStringOp, UndefOp, UndefinedOp, UnwrapOp, WrapOp, absence,
    admits_undefined, may_throw, optional_parts, optional_value, present_type, set_async,
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
use std::collections::{BTreeMap, HashMap, HashSet};

use builder::{Control, EXIT_STATUS, Finally, FunctionBuilder, Handler, LocalBinding};
use coerce::{Kind, KindExt, is_integer64, kind, void_type};
use methods::{CallExt, ReceiverExt};
use program::{Builtin, Signature};

/// The function `wasi:cli/run` binds to: the top-level statements of a module without exported
/// functions, otherwise an entry point awaiting module initialization.
pub const MAIN: &str = "perry_main";

pub use diagnostics::{LowerError, LowerErrorKind, ShapeError};
pub use perry_frontend::hir;

/// Parse TypeScript, lower it to Perry HIR, and build a verified-ready MIR module, with
/// `foreign` symbols by SDK name: WIT types and functions the program imports.
#[cfg(test)]
fn lower_source(
    ctx: &mut Context,
    source: &str,
    file: &str,
    foreign: &HashMap<String, hir::Foreign>,
) -> Result<ModuleOp> {
    let modules = source_files::load(source, file)?;
    lower_sources(ctx, source, file, &modules, foreign)
}

/// Lower in-memory entry source and preloaded `modules` into a verified MIR module.
pub fn lower_sources(
    ctx: &mut Context,
    source: &str,
    identity: &str,
    modules: &BTreeMap<String, String>,
    foreign: &HashMap<String, hir::Foreign>,
) -> Result<ModuleOp> {
    let program = perry_frontend::load_sources(ctx, source, identity, modules, foreign)?;
    lower_program(ctx, &program)
}

/// Elaborate a resolved frontend program into MIR.
pub fn lower_program(ctx: &mut Context, program: &perry_frontend::Program<'_>) -> Result<ModuleOp> {
    program::build_module(ctx, program.module(), program.foreign(), program.metadata())
}

#[cfg(test)]
mod tests;
