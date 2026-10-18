//! Perry HIR → perry-mir builder.
//!
//! One structural lowering with no representation decisions:
//! - every local (parameters included) is a `perry.local` slot, and pliron `mem2reg` builds SSA
//!   joins, loop-carried block arguments and later await continuations;
//! - contextual inference completes unresolved source types before MIR construction;
//! - tiers stay `Unassigned` and layouts abstract; later passes decide them.
//!
//! [lower_sources] resolves modules, passes the AST through [hir_compat], completes types
//! with [infer], and builds MIR with [program]. Each function body is lowered by a
//! [builder::FunctionBuilder], whose methods are spread over the modules below by topic.
//! Unsupported constructs are rejected with a diagnostic.
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
mod decoder;
mod expressions;
mod foreign;
mod hir_compat;
mod host;
mod infer;
mod json;
mod json_cursor;
mod methods;
mod modules;
mod operators;
mod program;
mod shapes;
pub mod source_files;
mod spread;
mod statements;
mod temporal;
mod url;
mod variants;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_hir::{
    ir::{
        BinaryOp, CompareOp, Expr, Function, LogicalOp, Module as HirModule, Stmt, UnaryOp,
        UpdateOp,
    },
    types::{FuncId, LocalId, Type as HirType},
};
use perry_mir::{
    AbortType, AllocOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
    ArrayTruncateOp, ArrayType, AsyncCallOp, AwaitOp, BigIntType, BinaryKind,
    BinaryOp as MirBinary, BrOp, BytesType, CallOp, CatchOp, CaughtOp, Combinator, CompareKind,
    CompareOp as MirCompare, ConcatOp, CondBrOp, DateType, ErrorKind, ErrorNewOp, ExceptionIsOp,
    ExceptionMessageOp, ExceptionType, ExitOp, FieldGetOp, FieldPresentOp, FieldSetOp, HeadersType,
    IncomingType, InstantType, Intrinsic, IntrinsicOp, IsNullOp, IsUndefinedOp, JsonStringifyOp,
    JsonType, LoadOp, LocalOp, NullOp, NumericCastOp, OmittedOp, OptionalCastOp, PlainDateTimeType,
    PlainDateType, PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp, PromiseType, RaiseOp,
    ReadResultType, ReadableType, ReaderType, RecordType, RequestType, ResponseType, RethrowOp,
    ReturnOp, SettledField, SettledFieldOp, SettledType, StatsType, StoreOp, StringConstOp,
    StringLenOp, StringType, TimerOp, ToNumberOp, ToStringOp, UndefOp, UndefinedOp, UnwrapOp,
    WrapOp, WritableType, WriterType, absence, may_throw, optional_parts, optional_value,
    set_async,
};
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic};
use pliron::printable::Printable as _;
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
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

/// HIR types and structural foreign (WIT) symbol descriptors.
pub mod hir {
    pub use perry_hir::types::{FunctionType, ObjectType, PropertyInfo, Type};
    pub use pliron::builtin::types::Signedness;

    /// A foreign (WIT) symbol in the world's symbol table.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Foreign {
        Type(Type),
        Integer { width: u32, signedness: Signedness },
        Resource(u32),
        Input(Box<Foreign>),
        Function(FunctionType),
    }
}
use builder::{
    Control, EXIT_STATUS, Finally, FunctionBuilder, Handler, LocalBinding, remove_unreachable,
};
use coerce::{Kind, kind, void_type};
use program::{Builtin, Signature};

/// The function holding a module's top-level statements, if it has any.
pub const MAIN: &str = "perry_main";

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

/// Lower only supplied contents; module identities never acquire host files.
pub fn lower_sources(
    ctx: &mut Context,
    source: &str,
    identity: &str,
    modules: &BTreeMap<String, String>,
    foreign: &HashMap<String, hir::Foreign>,
) -> Result<ModuleOp> {
    let loaded = modules::load(source, identity, modules, foreign)?;
    let mut lowered = hir_compat::lower(loaded, identity)?;
    infer::unresolved_types(ctx, &mut lowered.module, &lowered.metadata, foreign);
    program::build_module(ctx, &lowered.module, foreign, &lowered.metadata)
}

#[cfg(test)]
mod tests;
