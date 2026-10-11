//! Perry HIR → perry-mir builder.
//!
//! One structural lowering with no representation decisions:
//! - every local (parameters included) is a `perry.local` slot, and pliron `mem2reg` builds SSA
//!   joins, loop-carried block arguments and later await continuations;
//! - expression types come from the MIR types of the lowered operands, so there is no separate,
//!   recursive type inference;
//! - tiers stay `Unassigned` and layouts abstract; later passes decide them.
//!
//! [lower_source_with] runs the source passes (module graph, validation, stream rewriting),
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
mod json;
mod json_read;
mod modules;
mod operators;
mod program;
mod shapes;
mod source;
mod statements;
mod streams;
mod temporal;
mod url;
mod variants;
mod web;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_hir::{
    ir::{
        BinaryOp, CompareOp, Expr, Function, LogicalOp, Module as HirModule, Stmt, UnaryOp,
        UpdateOp,
    },
    types::{FuncId, LocalId, Type as HirType},
};
use perry_mir::{
    attrs::{BinaryKind, CompareKind, ConvertKind, ErrorKind},
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp, set_async},
    collections::{
        ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp, StringConcatOp, StringConstOp,
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
        UnwrapOp, WrapOp, absence, optional_value,
    },
    scalar::{BinaryOp as MirBinary, CompareOp as MirCompare, ConvertOp},
    types::{
        AbortType, ArrayType, BytesType, DateType, ExceptionType, HeadersType, IncomingType,
        InstantType, JsonType, PlainDateTimeType, PlainDateType, PromiseType, ReadResultType,
        ReadableType, ReaderType, RecordType, RequestType, ResponseType, SettledType, StatsType,
        StringType, WritableType, WriterType,
    },
};
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
    identifier::Identifier,
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
use std::collections::{HashMap, HashSet};

/// HIR types, for callers that describe foreign (WIT) types.
pub mod hir {
    pub use perry_hir::types::{FunctionType, ObjectType, PropertyInfo, Type};
}
use builder::{Control, EXIT_STATUS, Finally, FunctionBuilder, Handler, remove_unreachable};
use coerce::{Kind, kind};
use program::{Builtin, Signature};

/// The function holding a module's top-level statements, if it has any.
pub const MAIN: &str = "perry_main";

/// Parse TypeScript, lower it to Perry HIR, and build a verified-ready MIR module, with
/// `foreign` types by SDK name: WIT types the program imports from its generated declarations.
pub fn lower_source_with(
    ctx: &mut Context,
    source: &str,
    file: &str,
    foreign: &HashMap<String, HirType>,
) -> Result<ModuleOp> {
    let mut ast = modules::load(source, file, foreign)?;
    source::validate(&ast)?;
    streams::lower(&mut ast)?;
    decoder::preserve_options(&mut ast)?;
    array_methods::preserve_arguments(&mut ast);
    let hir = perry_hir::lower_module(&ast, "main", file)
        .map_err(|error| anyhow!("failed to lower {file}: {error:?}"))?;
    let spans = source::inferred_bindings(&ast);
    let inferred = hir
        .local_source_spans
        .iter()
        .filter_map(|(id, span)| spans.contains(&(span.start, span.end)).then_some(*id))
        .collect();
    program::build_module(ctx, &hir, foreign, inferred)
}

#[cfg(test)]
mod tests;
