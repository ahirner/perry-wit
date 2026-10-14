//! Perry HIR → perry-mir builder.
//!
//! One structural lowering with no representation decisions:
//! - every local (parameters included) is a `perry.local` slot, and pliron `mem2reg` builds SSA
//!   joins, loop-carried block arguments and later await continuations;
//! - expression types come from the MIR types of the lowered operands, so there is no separate,
//!   recursive type inference;
//! - tiers stay `Unassigned` and layouts abstract; later passes decide them.
//!
//! [lower_sources] runs the source passes (module graph, validation, stream rewriting),
//! Perry's HIR lowering, and then [program] over the HIR. Each function body is lowered by a
//! [builder::FunctionBuilder], whose methods are spread over the modules below by topic.
//! Unsupported constructs are rejected with a diagnostic.
mod aggregates;
mod array_methods;
mod builder;
mod builtins;
mod calls;
mod coerce;
mod control_flow;
mod dates;
mod decoder;
mod expressions;
mod foreign;
mod host;
mod infer;
mod json;
mod json_cursor;
mod locations;
mod modules;
mod operators;
mod program;
mod shapes;
mod source;
pub mod source_files;
mod spread;
mod statements;
mod streams;
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
    attrs::{BinaryKind, CompareKind, ErrorKind},
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp, set_async},
    collections::{
        ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp, ConcatOp, StringConstOp,
        StringLenOp,
    },
    control::{BrOp, CondBrOp, ReturnOp},
    exceptions::{
        CatchOp, CaughtOp, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExitOp, RaiseOp,
        RethrowOp,
    },
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    intrinsics::{
        Combinator, Intrinsic, IntrinsicOp, JsonStringifyOp, PromiseCombinatorOp,
        PromiseRejectedOp, PromiseResolvedOp, SettledField, SettledFieldOp, may_throw,
    },
    locals::{LoadOp, LocalOp, StoreOp, UndefOp},
    optional::{
        IsNullOp, IsUndefinedOp, NullOp, OptionalCastOp, ToNumberOp, ToStringOp, UndefinedOp,
        UnwrapOp, WrapOp, absence, optional_parts, optional_value,
    },
    scalar::{BinaryOp as MirBinary, CompareOp as MirCompare, NumericCastOp},
    types::{
        AbortType, ArrayType, BytesType, DateType, ExceptionType, HeadersType, IncomingType,
        InstantType, JsonType, PlainDateTimeType, PlainDateType, PromiseType, ReadResultType,
        ReadableType, ReaderType, RecordType, RequestType, ResponseType, SettledType, StatsType,
        StringType, WritableType, WriterType,
    },
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
use shapes::{Shapes, identifier};
use std::collections::{BTreeMap, HashMap, HashSet};

/// HIR types, for callers that describe foreign (WIT) types.
pub mod hir {
    pub use perry_hir::types::{FunctionType, ObjectType, PropertyInfo, Type};
}
use builder::{Control, EXIT_STATUS, Finally, FunctionBuilder, Handler, remove_unreachable};
use coerce::{Kind, kind, void_type};
use program::{Builtin, Signature};

/// The function holding a module's top-level statements, if it has any.
pub const MAIN: &str = "perry_main";

/// Parse TypeScript, lower it to Perry HIR, and build a verified-ready MIR module, with
/// `foreign` types by SDK name: WIT types the program imports from its generated declarations.
#[cfg(test)]
fn lower_source(
    ctx: &mut Context,
    source: &str,
    file: &str,
    foreign: &HashMap<String, HirType>,
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
    foreign: &HashMap<String, HirType>,
) -> Result<ModuleOp> {
    let loaded = modules::load(source, identity, modules, foreign)?;
    let mut ast = loaded.module;
    let mut locations = loaded.locations;
    locations.collect(&ast);
    source::validate(&ast)?;
    streams::lower(&mut ast)?;
    decoder::preserve_options(&mut ast)?;
    array_methods::preserve_arguments(&mut ast);
    let mut hir = perry_hir::lower_module(&ast, "main", identity).map_err(|error| {
        let span = error
            .downcast_ref::<perry_hir::error::LowerError>()
            .and_then(|error| error.span);
        let diagnostic = PerryDiagnostic::error(
            DiagnosticCode::UnsupportedExpression,
            format!("failed to lower {identity}: {error:#}"),
        );
        let error = error.context(diagnostic);
        match span {
            Some(span) => locations.annotate(error, span),
            None => error,
        }
    })?;
    let spans = source::inferred_bindings(&ast);
    let inferred = hir
        .local_source_spans
        .iter()
        .filter_map(|(id, span)| spans.contains(&(span.start, span.end)).then_some(*id))
        .collect();
    infer::unresolved_types(&mut hir, &inferred, &locations, foreign);
    program::build_module(ctx, &hir, foreign, inferred, &locations)
}

#[cfg(test)]
mod tests;
