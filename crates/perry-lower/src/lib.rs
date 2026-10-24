//! Perry HIR → perry-mir builder.
//!
//! Structural lowering from HIR into MIR follows three principles:
//! - Every local (including parameters) lowers to a `perry.local` slot, and pliron `mem2reg`
//!   constructs SSA joins, loop-carried block arguments, and await continuations.
//! - Contextual inference completes unresolved source types before MIR construction.
//! - Allocation tiers start `Unassigned` and layouts remain abstract for downstream MIR and codegen passes.
//!
//! [lower_sources] resolves modules, passes the AST through `hir_compat`, completes types
//! with `infer`, and builds MIR with `program`. Each function body is lowered by a
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
mod expressions;
mod foreign;
mod hir_compat;
mod host;
mod infer;
mod inline;
mod json;
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
    AllocOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp, ArrayTruncateOp,
    ArrayType, AsyncCallOp, AwaitOp, BigIntType, BinaryKind, BinaryOp as MirBinary, BrOp,
    BytesType, CallOp, CatchOp, CaughtOp, Combinator, CompareKind, CompareOp as MirCompare,
    ConcatOp, CondBrOp, ErrorKind, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExceptionType,
    ExitOp, FieldGetOp, FieldPresentOp, FieldSetOp, Intrinsic, IsNullOp, IsUndefinedOp,
    JsonStringifyOp, JsonType, LoadOp, NullOp, NumericCastOp, OmittedOp, OptionalCastOp,
    PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp, PromiseType, RaiseOp, ReaderType,
    RecordKind, RecordType, RethrowOp, ReturnOp, SettledField, SettledFieldOp, StoreOp,
    StringLenOp, StringType, TimerOp, ToNumberOp, ToStringOp, UndefOp, UndefinedOp, UnwrapOp,
    WrapOp, absence, admits_undefined, may_throw, optional_parts, optional_value, present_type,
    set_async,
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
use shapes::{BindingType, HirTypeExt, Shapes, identifier};
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
use builder::{Control, EXIT_STATUS, Finally, FunctionBuilder, Handler, LocalBinding};
use coerce::{Kind, is_integer64, kind, void_type};
use program::{Builtin, Signature};

/// The function `wasi:cli/run` binds to: the top-level statements of a module without exported
/// functions, otherwise an entry point awaiting module initialization.
pub const MAIN: &str = "perry_main";

/// Type-shape and coercion errors during HIR-to-MIR lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    MissingTypeAnnotation,
    UnsupportedType { ty: String },
    IncompatibleRecordLayout { actual: String, expected: String },
    AbsenceMismatch { actual: String, expected: String },
    TypeMismatch { actual: String, expected: String },
}

impl ShapeError {
    pub fn help(&self) -> Option<&'static str> {
        match self {
            Self::MissingTypeAnnotation => Some(
                "Add a concrete type annotation to this binding, parameter, or function return.",
            ),
            _ => None,
        }
    }
}

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTypeAnnotation => {
                f.write_str("the value type cannot be inferred for the MIR backend")
            }
            Self::UnsupportedType { ty } => {
                write!(f, "MIR backend does not support type {ty} yet")
            }
            Self::IncompatibleRecordLayout { actual, expected }
            | Self::AbsenceMismatch { actual, expected }
            | Self::TypeMismatch { actual, expected } => {
                write!(
                    f,
                    "value type {actual} differs from the expected type {expected}"
                )
            }
        }
    }
}

/// Domain error kinds emitted by the HIR-to-MIR lowerer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerErrorKind {
    Parse {
        identity: String,
        message: String,
    },
    ModuleResolution {
        name: String,
        parent: std::path::PathBuf,
    },
    ImportSignatureMismatch {
        binding: String,
        expected: usize,
        actual: usize,
    },
    Shape(ShapeError),
    MissingRequiredField {
        fields: Vec<String>,
    },
    UnsupportedOperator {
        op: String,
    },
    UnsupportedSpread,
    UnsupportedExpression(String),
}

impl From<ShapeError> for LowerErrorKind {
    fn from(error: ShapeError) -> Self {
        Self::Shape(error)
    }
}

impl LowerErrorKind {
    pub fn help(&self) -> Option<&'static str> {
        match self {
            Self::Shape(shape) => shape.help(),
            _ => None,
        }
    }
}

impl std::fmt::Display for LowerErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { identity, message } => write!(f, "Parsing {identity}: {message}"),
            Self::ModuleResolution { name, parent } => {
                write!(f, "Cannot resolve '{name}' from {}", parent.display())
            }
            Self::ImportSignatureMismatch {
                binding,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "WIT import {binding} expects {expected} arguments, got {actual}"
                )
            }
            Self::Shape(shape) => shape.fmt(f),
            Self::MissingRequiredField { fields } => {
                write!(f, "missing required record fields: {}", fields.join(", "))
            }
            Self::UnsupportedOperator { op } => {
                write!(f, "MIR backend does not support operator {op} yet")
            }
            Self::UnsupportedSpread => {
                f.write_str("sparse array spreads are outside the dense-array profile")
            }
            Self::UnsupportedExpression(message) => f.write_str(message),
        }
    }
}

/// Source-spanned error emitted during HIR-to-MIR lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LowerError {
    pub kind: LowerErrorKind,
    pub span: Option<perry_mir::SourceSpan>,
    pub notes: Vec<String>,
}

impl LowerError {
    pub fn new(kind: impl Into<LowerErrorKind>) -> Self {
        Self {
            kind: kind.into(),
            span: None,
            notes: Vec::new(),
        }
    }

    pub fn with_span(mut self, span: perry_mir::SourceSpan) -> Self {
        self.span = Some(span);
        self
    }

    pub fn help(&self) -> Option<&'static str> {
        self.kind.help()
    }

    /// Extract a [`LowerError`] from `error`, prepending any outer `anyhow` context notes.
    pub fn try_from_anyhow(error: anyhow::Error) -> std::result::Result<Self, anyhow::Error> {
        if error.downcast_ref::<Self>().is_none() {
            return Err(error);
        }
        let mut prefix: Vec<String> = error
            .chain()
            .take_while(|cause| cause.downcast_ref::<Self>().is_none())
            .map(ToString::to_string)
            .collect();
        let mut diagnostic = error.downcast::<Self>()?;
        if !prefix.is_empty() {
            prefix.append(&mut diagnostic.notes);
            diagnostic.notes = prefix;
        }
        Ok(diagnostic)
    }
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for LowerError {}

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
    let loaded = modules::load(source, identity, modules, foreign)?;
    let mut lowered = hir_compat::lower(loaded, identity)?;
    infer::specialize_functions(ctx, &mut lowered.module, &lowered.metadata, foreign)?;
    infer::unresolved_types(ctx, &mut lowered.module, &lowered.metadata, foreign);
    program::build_module(ctx, &lowered.module, foreign, &lowered.metadata)
}

#[cfg(test)]
mod tests;
