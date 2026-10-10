//! Perry HIR → perry-mir builder (`TODOs.md` P2.2).
//!
//! One structural lowering with no representation decisions:
//! - every local (parameters included) is a `perry.local` slot, and pliron `mem2reg` builds SSA
//!   joins, loop-carried block arguments and later await continuations;
//! - expression types come from the MIR types of the lowered operands, so there is no separate,
//!   recursive type inference;
//! - tiers stay `Unassigned` and layouts abstract; later passes decide them.
//!
//! Supported so far: synchronous functions over `number`, `boolean`, `string`, records
//! (interfaces, type literals and object literals of the same shape) and dense arrays; locals,
//! assignment and `++`/`--`; arithmetic, comparisons, `&&`/`||`, `!`, unary `-`/`+`, `?:`;
//! `if`, `while`, `do`/`while`, `for`, unlabeled `break`/`continue`, `return`; direct calls;
//! string concatenation; field reads and writes; array literals, `push`, indexing and
//! `.length`. Everything else is rejected with a diagnostic.
mod control_flow;
mod dates;
mod decoder;
mod foreign;
mod json;
mod modules;
mod shapes;
mod source;
mod streams;
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
        JsonType, PromiseType, ReadResultType, ReadableType, ReaderType, RecordType, RequestType,
        ResponseType, SettledType, StatsType, StringType, WritableType, WriterType,
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

/// Parse TypeScript, lower it to Perry HIR, and build a verified-ready MIR module.
pub fn lower_source(ctx: &mut Context, source: &str, file: &str) -> Result<ModuleOp> {
    lower_source_with(ctx, source, file, &HashMap::new())
}

/// [lower_source] with `foreign` types by SDK name: WIT types the program imports from its
/// generated declarations.
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
    let hir = perry_hir::lower_module(&ast, "main", file)
        .map_err(|error| anyhow!("failed to lower {file}: {error:?}"))?;
    let spans = source::inferred_bindings(&ast);
    let inferred = hir
        .local_source_spans
        .iter()
        .filter_map(|(id, span)| spans.contains(&(span.start, span.end)).then_some(*id))
        .collect();
    build_module(ctx, &hir, foreign, inferred)
}

struct Signature {
    name: Identifier,
    /// `async`: callers start it with `perry.async_call` and get a promise of `results[0]`.
    is_async: bool,
    params: Vec<TypeHandle>,
    results: Vec<TypeHandle>,
}

/// The function holding a module's top-level statements, if it has any.
pub const MAIN: &str = "perry_main";

/// Build a MIR module from HIR functions and, as [MAIN], the top-level statements.
pub fn lower_module(ctx: &mut Context, hir: &HirModule) -> Result<ModuleOp> {
    lower_module_with(ctx, hir, &HashMap::new())
}

/// [lower_module] with foreign types (see [lower_source_with]).
pub fn lower_module_with(
    ctx: &mut Context,
    hir: &HirModule,
    foreign: &HashMap<String, HirType>,
) -> Result<ModuleOp> {
    build_module(ctx, hir, foreign, HashSet::new())
}

fn build_module(
    ctx: &mut Context,
    hir: &HirModule,
    foreign: &HashMap<String, HirType>,
    inferred: HashSet<LocalId>,
) -> Result<ModuleOp> {
    let module = ModuleOp::new(ctx, identifier(&hir_module_name(hir))?);
    let shapes = Shapes::new(module, hir, foreign, inferred);
    // Imported built-ins by local name: `setTimeout` from `node:timers/promises`, and the
    // functions of `node:fs/promises`.
    let mut builtins: HashMap<String, Builtin> = HashMap::new();
    for import in &hir.imports {
        let source = import.source.as_str();
        let module = match source {
            "timers/promises" | "node:timers/promises" => Some(BuiltinModule::Timers),
            "fs/promises" | "node:fs/promises" => Some(BuiltinModule::Fs),
            "stream" | "node:stream" => Some(BuiltinModule::Stream),
            "perry:http-server" => Some(BuiltinModule::HttpServer),
            _ => None,
        };
        let builtin = source.starts_with("node:")
            || matches!(
                source,
                "timers" | "fs" | "fs/promises" | "stream" | "process" | "crypto"
            );
        ensure!(
            !matches!(source, "fs" | "node:fs"),
            "Unsupported filesystem import {source}: synchronous and callback APIs are not supported; use node:fs/promises"
        );
        ensure!(
            module.is_some() || !builtin,
            "Unsupported capability import {source}: only node:timers/promises, node:fs/promises and Writable from node:stream are supported so far"
        );
        let Some(module) = module else { continue };
        for specifier in &import.specifiers {
            let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                bail!("only named imports from {source} are supported yet");
            };
            let builtin = Builtin::from_import(module, imported)
                .with_context(|| format!("{imported} from {source} is not supported yet"))?;
            builtins.insert(local.clone(), builtin);
        }
    }
    let mut signatures = HashMap::new();
    for function in &hir.functions {
        ensure!(
            !function.is_generator,
            "generator function {} is not supported yet",
            function.name
        );

        let params = function
            .params
            .iter()
            .map(|param| {
                ensure!(
                    param.default.is_none() && !param.is_rest,
                    "default and rest parameters are not supported yet: {}",
                    function.name
                );
                shapes
                    .value_type(ctx, &param.ty)
                    .with_context(|| format!("parameter {} of {}", param.name, function.name))
            })
            .collect::<Result<Vec<_>>>()?;
        let results = match (&function.return_type, function.is_async) {
            (HirType::Void, false) => vec![],
            // An async function settles its promise with its result; `void` settles `undefined`.
            (HirType::Promise(inner), true) => vec![shapes.settled_type(ctx, inner)?],
            (ty, true) => vec![shapes.settled_type(ctx, ty)?],
            (ty, false) => {
                vec![
                    shapes
                        .value_type(ctx, ty)
                        .with_context(|| format!("result of {}", function.name))?,
                ]
            }
        };
        signatures.insert(
            function.id,
            Signature {
                is_async: function.is_async,
                name: identifier(&function.name)?,
                params,
                results,
            },
        );
    }
    // Module initialization of named built-in imports (`Writable` from `node:stream`) has
    // nothing to run; without other statements there is no main function.
    let init: Vec<Stmt> = hir
        .init
        .iter()
        .filter(|statement| {
            !matches!(statement, Stmt::Expr(Expr::Call { callee, .. })
                if matches!(&**callee, Expr::ExternFuncRef { name, .. }
                    if name == "js_native_module_named_esm_export_value"))
        })
        .cloned()
        .collect();
    if !init.is_empty() {
        // Top-level statements (with top-level `await`) form the async main function of a
        // command (`wasi:cli/run`), settling `undefined`.
        let persistent = hir.functions.iter().any(|f| f.is_exported);
        let signature = Signature {
            name: identifier(if persistent {
                perry_mir::globals::INITIALIZER
            } else {
                MAIN
            })?,
            is_async: !persistent,
            params: vec![],
            results: if persistent {
                vec![]
            } else {
                vec![shapes.settled_type(ctx, &HirType::Void)?]
            },
        };
        let func = FunctionBuilder::lower_body(
            ctx,
            &signature,
            &[],
            &init,
            &signatures,
            &shapes,
            &builtins,
        )
        .context("lowering top-level statements")?;
        if signature.is_async {
            set_async(ctx, &func);
        }
        module.append_operation(ctx, func.get_operation(), 0);
    }
    for function in &hir.functions {
        let func = FunctionBuilder::lower(ctx, function, &signatures, &shapes, &builtins)
            .with_context(|| format!("lowering function {}", function.name))?;
        if function.is_exported {
            perry_mir::calls::set_exported(ctx, &func);
        }
        if function.is_async {
            set_async(ctx, &func);
        }
        module.append_operation(ctx, func.get_operation(), 0);
    }
    Ok(module)
}

/// Delete blocks no path from the entry reaches (code after `return`, handlers nothing throws
/// to); they would break dominance of the values they use.
fn remove_unreachable(ctx: &mut Context, region: Ptr<pliron::region::Region>) {
    let blocks: Vec<_> = region.deref(ctx).iter(ctx).collect();
    let mut reached = HashSet::from([blocks[0]]);
    let mut stack = vec![blocks[0]];
    while let Some(block) = stack.pop() {
        for succ in block.deref(ctx).succs(ctx) {
            if reached.insert(succ) {
                stack.push(succ);
            }
        }
    }
    let dead: Vec<_> = blocks
        .into_iter()
        .filter(|b| !reached.contains(b))
        .collect();
    for &block in &dead {
        BasicBlock::drop_all_uses(block, ctx);
    }
    for block in dead {
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        for op in ops.into_iter().rev() {
            Operation::erase(op, ctx);
        }
    }
    let dead: Vec<_> = region
        .deref(ctx)
        .iter(ctx)
        .filter(|b| !reached.contains(b))
        .collect();
    for block in dead {
        BasicBlock::erase(block, ctx);
    }
}

fn hir_module_name(hir: &HirModule) -> String {
    let name: String = hir
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("m_{name}")
    } else {
        name
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    BigInt,
    Url,
    SearchParams,
    Number,
    Boolean,
    String,
    Record,
    Variant,
    NumberString,
    Resource,
    Array,
    Exception,
    Bytes,
    ByteBuffer,
    ByobReader,
    Json,
    Date,
    Decoder,
    Regex,
    Settled,
    Stats,
    Headers,
    Response,
    Request,
    Writable,
    Writer,
    Readable,
    Reader,
    ReadResult,
    Incoming,
    Abort,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BuiltinModule {
    Timers,
    Fs,
    Stream,
    /// `perry:http-server`: the services the compiler's `wasi:http/handler` adapter uses.
    HttpServer,
}

/// A function imported from a built-in module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Builtin {
    SetTimeout,
    ReadFile,
    WriteFile,
    Stat,
    Readdir,
    Mkdir,
    Unlink,
    Rmdir,
    /// The `Writable` class, for `Writable.toWeb`.
    Writable,
    /// A service of `perry:http-server`.
    Serve(Intrinsic),
}

impl Builtin {
    fn from_import(module: BuiltinModule, name: &str) -> Option<Self> {
        Some(match (module, name) {
            (BuiltinModule::Timers, "setTimeout") => Builtin::SetTimeout,
            (BuiltinModule::Fs, "readFile") => Builtin::ReadFile,
            (BuiltinModule::Fs, "writeFile") => Builtin::WriteFile,
            (BuiltinModule::Fs, "stat") => Builtin::Stat,
            (BuiltinModule::Fs, "readdir") => Builtin::Readdir,
            (BuiltinModule::Fs, "mkdir") => Builtin::Mkdir,
            (BuiltinModule::Fs, "unlink") => Builtin::Unlink,
            (BuiltinModule::Fs, "rmdir") => Builtin::Rmdir,
            (BuiltinModule::Stream, "Writable") => Builtin::Writable,
            (BuiltinModule::HttpServer, name) => Builtin::Serve(match name {
                "request" => Intrinsic::ServeRequest,
                "body" => Intrinsic::ServeBody,
                "method" => Intrinsic::ServeMethod,
                "methodOther" => Intrinsic::ServeMethodOther,
                "hasScheme" => Intrinsic::ServeHasScheme,
                "scheme" => Intrinsic::ServeScheme,
                "schemeOther" => Intrinsic::ServeSchemeOther,
                "authority" => Intrinsic::ServeAuthority,
                "path" => Intrinsic::ServePath,
                "headerCount" => Intrinsic::ServeHeaderCount,
                "headerName" => Intrinsic::ServeHeaderName,
                "headerValue" => Intrinsic::ServeHeaderValue,
                "addHeader" => Intrinsic::ServeAddHeader,
                "respond" => Intrinsic::ServeRespond,
                _ => return None,
            }),
            _ => return None,
        })
    }
}

fn kind(ctx: &Context, value: Value) -> Result<Kind> {
    let ty = value.get_type(ctx);
    let ty = ty.deref(ctx);
    if ty.is::<perry_mir::types::BigIntType>() {
        Ok(Kind::BigInt)
    } else if ty.is::<FP64Type>() {
        Ok(Kind::Number)
    } else if ty.is::<StringType>() {
        Ok(Kind::String)
    } else if ty.is::<perry_mir::types::ResourceType>() {
        Ok(Kind::Resource)
    } else if ty.is::<perry_mir::types::NumberStringType>() {
        Ok(Kind::NumberString)
    } else if ty.is::<perry_mir::types::VariantType>() {
        Ok(Kind::Variant)
    } else if ty.is::<RecordType>() {
        Ok(Kind::Record)
    } else if ty.is::<ArrayType>() {
        Ok(Kind::Array)
    } else if ty.is::<ExceptionType>() {
        Ok(Kind::Exception)
    } else if ty.is::<BytesType>() {
        Ok(Kind::Bytes)
    } else if ty.is::<perry_mir::types::UrlType>() {
        Ok(Kind::Url)
    } else if ty.is::<perry_mir::types::SearchParamsType>() {
        Ok(Kind::SearchParams)
    } else if ty.is::<JsonType>() {
        Ok(Kind::Json)
    } else if ty.is::<perry_mir::types::RegexType>() {
        Ok(Kind::Regex)
    } else if ty.is::<perry_mir::types::DecoderType>() {
        Ok(Kind::Decoder)
    } else if ty.is::<DateType>() {
        Ok(Kind::Date)
    } else if ty.is::<SettledType>() {
        Ok(Kind::Settled)
    } else if ty.is::<StatsType>() {
        Ok(Kind::Stats)
    } else if ty.is::<HeadersType>() {
        Ok(Kind::Headers)
    } else if ty.is::<ResponseType>() {
        Ok(Kind::Response)
    } else if ty.is::<RequestType>() {
        Ok(Kind::Request)
    } else if ty.is::<WritableType>() {
        Ok(Kind::Writable)
    } else if ty.is::<WriterType>() {
        Ok(Kind::Writer)
    } else if ty.is::<ReadableType>() {
        Ok(Kind::Readable)
    } else if ty.is::<perry_mir::types::ByteBufferType>() {
        Ok(Kind::ByteBuffer)
    } else if ty.is::<perry_mir::types::ByobReaderType>() {
        Ok(Kind::ByobReader)
    } else if ty.is::<ReaderType>() {
        Ok(Kind::Reader)
    } else if ty.is::<AbortType>() {
        Ok(Kind::Abort)
    } else if ty.is::<IncomingType>() {
        Ok(Kind::Incoming)
    } else if ty.is::<ReadResultType>() {
        Ok(Kind::ReadResult)
    } else if ty
        .downcast_ref::<IntegerType>()
        .is_some_and(|int| int.width() == 1)
    {
        Ok(Kind::Boolean)
    } else {
        bail!("unsupported value type in expression")
    }
}

enum Control {
    Loop {
        continue_to: Ptr<BasicBlock>,
        break_to: Ptr<BasicBlock>,
    },
    Switch {
        break_to: Ptr<BasicBlock>,
    },
}

/// The innermost enclosing `catch` or exceptional `finally` path: throwing ops store their
/// status in `status` and branch to `block`.
#[derive(Clone, Copy)]
struct Handler {
    block: Ptr<BasicBlock>,
    status: Value,
}

/// An enclosing `finally`, run inline before `return`, `break` and `continue` leave it.
#[derive(Clone)]
struct Finally {
    body: Vec<Stmt>,
    /// `handlers.len()` and `controls.len()` outside the `try`.
    handlers: usize,
    controls: usize,
}

/// Status of `process.exit` (`ErrorKind::Exit`): handlers pass it on.
const EXIT_STATUS: u64 = 5;

struct FunctionBuilder<'s> {
    region: Ptr<pliron::region::Region>,
    ins: IRInserter<DummyListener>,
    block: Ptr<BasicBlock>,
    locals: HashMap<LocalId, Value>,
    global_slots: HashSet<Value>,
    /// Iterator-protocol guards of array destructuring that are statically false: the
    /// branches they select are not lowered.
    false_guards: HashSet<LocalId>,
    variant_guards: HashMap<variants::Path, Vec<(String, bool)>>,
    controls: Vec<Control>,
    handlers: Vec<Handler>,
    finallies: Vec<Finally>,
    signatures: &'s HashMap<FuncId, Signature>,
    shapes: &'s Shapes<'s>,
    /// Imported built-ins by local name.
    builtins: &'s HashMap<String, Builtin>,
    results: Vec<TypeHandle>,
    is_async: bool,
    labels: usize,
}

impl<'s> FunctionBuilder<'s> {
    fn lower(
        ctx: &mut Context,
        function: &Function,
        signatures: &'s HashMap<FuncId, Signature>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<String, Builtin>,
    ) -> Result<FuncOp> {
        ensure!(
            function
                .captures
                .iter()
                .all(|id| shapes.globals.borrow().contains_key(id)),
            "closures capturing activation-local values are unsupported: {}",
            function.name
        );
        let signature = &signatures[&function.id];
        let params: Vec<LocalId> = function.params.iter().map(|p| p.id).collect();
        Self::lower_body(
            ctx,
            signature,
            &params,
            &function.body,
            signatures,
            shapes,
            builtins,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_body(
        ctx: &mut Context,
        signature: &Signature,
        params: &[LocalId],
        body: &[Stmt],
        signatures: &'s HashMap<FuncId, Signature>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<String, Builtin>,
    ) -> Result<FuncOp> {
        let ty = FunctionType::get(ctx, signature.params.clone(), signature.results.clone());
        let func = FuncOp::new(ctx, signature.name.clone(), ty);
        let entry = func.get_entry_block(ctx);
        let region = func.get_operation().deref(ctx).get_region(0);
        let mut builder = Self::new(ctx, &func, signature, signatures, shapes, builtins);
        let mut globals: Vec<_> = shapes
            .globals
            .borrow()
            .iter()
            .map(|(&id, &ty)| (id, ty))
            .collect();
        globals.sort_by_key(|(id, _)| *id);
        for (id, ty) in globals {
            let slot = builder.emit_value(ctx, |ctx| {
                perry_mir::globals::GlobalOp::new(
                    ctx,
                    format!("global_{id}").try_into().expect("global name"),
                    ty,
                )
            });
            builder.locals.insert(id, slot);
            builder.global_slots.insert(slot);
        }
        for (index, param) in params.iter().enumerate() {
            let value = entry.deref(ctx).get_argument(index);
            let slot = builder.slot(ctx, value.get_type(ctx));
            builder.store(ctx, slot, value);
            builder.locals.insert(*param, slot);
        }
        builder.statements(ctx, body)?;
        if !builder.terminated(ctx) {
            let values = builder.implicit_results(ctx);
            builder.emit(ctx, |ctx| ReturnOp::new(ctx, values));
        }
        remove_unreachable(ctx, region);
        Ok(func)
    }

    fn new(
        ctx: &Context,
        func: &FuncOp,
        signature: &Signature,
        signatures: &'s HashMap<FuncId, Signature>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<String, Builtin>,
    ) -> Self {
        let entry = func.get_entry_block(ctx);
        let region = func.get_operation().deref(ctx).get_region(0);
        Self {
            region,
            ins: IRInserter::new_at_block_end(entry),
            block: entry,
            locals: HashMap::new(),
            global_slots: HashSet::new(),
            false_guards: HashSet::new(),
            variant_guards: HashMap::new(),
            controls: Vec::new(),
            handlers: Vec::new(),
            finallies: Vec::new(),
            signatures,
            shapes,
            builtins,
            results: signature.results.clone(),
            is_async: signature.is_async,
            labels: 0,
        }
    }

    /// `setTimeout(delay?, value?)` from `node:timers/promises`.
    fn timer(&mut self, ctx: &mut Context, args: &[Expr]) -> Result<Value> {
        ensure!(
            args.len() <= 3,
            "setTimeout takes a delay, a value and options"
        );
        let delay = match args.first() {
            Some(delay) => {
                let value = self.expression(ctx, delay)?;
                let value = self.arithmetic(ctx, value);
                ensure!(
                    kind(ctx, value)? == Kind::Number,
                    "setTimeout delay must be a number"
                );
                value
            }
            None => self.number(ctx, 1.0),
        };
        let value = match args.get(1) {
            Some(value) => {
                let value = self.expression(ctx, value)?;
                self.defined(ctx, value)?
            }
            None => {
                let number = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number))
            }
        };
        // `{ signal }`: the only option (`ref` has no meaning in a component).
        let signal = match args.get(2) {
            None => None,
            Some(options) => {
                let signal = match options {
                    Expr::New {
                        class_name, args, ..
                    } if self
                        .shapes
                        .literal_fields(class_name)
                        .is_some_and(|names| names == ["signal"]) =>
                    {
                        &args[0]
                    }
                    Expr::Object(fields) if fields.len() == 1 && fields[0].0 == "signal" => {
                        &fields[0].1
                    }
                    _ => bail!("setTimeout options must be a literal {{ signal }}"),
                };
                Some(self.typed_operand(ctx, signal, Kind::Abort, "an AbortSignal")?)
            }
        };
        Ok(self.emit_value(ctx, |ctx| TimerOp::new(ctx, delay, value, signal)))
    }

    /// `new Headers(init?)`, `new Response(body?, init?)`, `new Request(url, init?)`.
    fn web_new(&mut self, ctx: &mut Context, class: &str, args: &[Expr]) -> Result<Value> {
        if class == "Headers" {
            ensure!(args.len() <= 1, "new Headers takes one init");
            return self.headers_init(ctx, args.first());
        }
        let (first, init) = (args.first(), args.get(1));
        ensure!(args.len() <= 2, "{class} takes two arguments");
        let fields = self.init_fields(init)?;
        let field = |name: &str| fields.iter().find(|(n, _)| n == name).map(|(_, e)| *e);
        for (name, _) in &fields {
            let known: &[&str] = if class == "Response" {
                &["status", "statusText", "headers"]
            } else {
                &["method", "headers", "body"]
            };
            ensure!(
                known.contains(&name.as_str()),
                "{class} init field {name} is not supported yet"
            );
        }
        let headers = self.headers_init(ctx, field("headers"))?;
        let body = if class == "Response" {
            first
        } else {
            field("body")
        };
        let (body, text) = self.body_init(ctx, body)?;
        let text = self.boolean(ctx, text);
        if class == "Response" {
            let status = self.number_or(ctx, field("status"), 200.0)?;
            let status_text = match field("statusText") {
                Some(expr) => self.typed_operand(ctx, expr, Kind::String, "a status text")?,
                None => self.string(ctx, ""),
            };
            let operands = vec![body, text, status, status_text, headers];
            return Ok(self
                .intrinsic(ctx, Intrinsic::ResponseNew, operands)
                .expect("response"));
        }
        let url = first.context("new Request needs a URL")?;
        let url = self.typed_operand(ctx, url, Kind::String, "a URL string")?;
        let method = match field("method") {
            Some(expr) => self.typed_operand(ctx, expr, Kind::String, "a method string")?,
            None => self.string(ctx, "GET"),
        };
        let operands = vec![url, method, body, text, headers];
        Ok(self
            .intrinsic(ctx, Intrinsic::RequestNew, operands)
            .expect("request"))
    }

    /// The fields of an init object literal, by name.
    fn init_fields<'e>(&self, init: Option<&'e Expr>) -> Result<Vec<(String, &'e Expr)>> {
        match init {
            None | Some(Expr::Undefined) => Ok(Vec::new()),
            Some(Expr::New {
                class_name, args, ..
            }) if self.shapes.literal_fields(class_name).is_some() => {
                let names = self.shapes.literal_fields(class_name).expect("literal");
                Ok(names.into_iter().zip(args.iter()).collect())
            }
            Some(_) => bail!("init objects must be object literals yet"),
        }
    }

    /// A header list from an init: absent, an object literal of string values, or `Headers`.
    fn headers_init(&mut self, ctx: &mut Context, init: Option<&Expr>) -> Result<Value> {
        if let Some(expr) = init
            && !matches!(expr, Expr::Undefined | Expr::New { .. })
        {
            let value = self.expression(ctx, expr)?;
            ensure!(
                kind(ctx, value)? == Kind::Headers,
                "headers init must be an object literal or Headers"
            );
            return Ok(self
                .intrinsic(ctx, Intrinsic::HeadersCopy, vec![value])
                .expect("headers"));
        }
        let fields = self.init_fields(init)?;
        let headers = self
            .intrinsic(ctx, Intrinsic::HeadersNew, vec![])
            .expect("headers");
        for (name, expr) in fields {
            let name = self.string(ctx, &name);
            let value = self.typed_operand(ctx, expr, Kind::String, "a header value")?;
            self.intrinsic(ctx, Intrinsic::HeadersAppend, vec![headers, name, value]);
        }
        Ok(headers)
    }

    /// A body init as `Uint8Array | undefined` and whether it was a string.
    fn body_init(&mut self, ctx: &mut Context, body: Option<&Expr>) -> Result<(Value, bool)> {
        let bytes: TypeHandle = BytesType::get(ctx).into();
        let Some(body) = body.filter(|b| !matches!(b, Expr::Undefined | Expr::Null)) else {
            return Ok((
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, bytes)),
                false,
            ));
        };
        let value = self.expression(ctx, body)?;
        let value = self.defined(ctx, value)?;
        let (value, text) = match kind(ctx, value)? {
            Kind::Bytes => (value, false),
            Kind::String => {
                let encoded = self
                    .intrinsic(ctx, Intrinsic::TextEncode, vec![value])
                    .expect("bytes");
                (encoded, true)
            }
            _ => bail!("bodies are strings and Uint8Arrays yet"),
        };
        Ok((self.optional(ctx, value), text))
    }

    /// A property (`args` is `None`) or method of `Headers`, `Response` or `Request`.
    fn web_member(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        name: &str,
        args: Option<&[Expr]>,
    ) -> Result<Value> {
        let receiver_kind = kind(ctx, receiver)?;
        let strings = |builder: &mut Self, ctx: &mut Context, count: usize| -> Result<Vec<Value>> {
            let args = args.unwrap_or(&[]);
            ensure!(args.len() == count, "{name} takes {count} arguments");
            args.iter()
                .map(|arg| builder.typed_operand(ctx, arg, Kind::String, "a string"))
                .collect()
        };
        let (intrinsic, operands) = match (receiver_kind, name, args.is_some()) {
            (Kind::Headers, "append" | "set", true) => {
                let mut operands = vec![receiver];
                operands.extend(strings(self, ctx, 2)?);
                let intrinsic = if name == "append" {
                    Intrinsic::HeadersAppend
                } else {
                    Intrinsic::HeadersSet
                };
                self.intrinsic(ctx, intrinsic, operands);
                // Both return `undefined`; statements discard it.
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (Kind::Headers, "delete", true) => {
                let mut operands = vec![receiver];
                operands.extend(strings(self, ctx, 1)?);
                self.intrinsic(ctx, Intrinsic::HeadersDelete, operands);
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (Kind::Headers, "get" | "has", true) => {
                let mut operands = vec![receiver];
                operands.extend(strings(self, ctx, 1)?);
                let intrinsic = if name == "get" {
                    Intrinsic::HeadersGet
                } else {
                    Intrinsic::HeadersHas
                };
                (intrinsic, operands)
            }
            (Kind::Response, "status", false) => (Intrinsic::ResponseStatus, vec![receiver]),
            (Kind::Response, "statusText", false) => {
                (Intrinsic::ResponseStatusText, vec![receiver])
            }
            (Kind::Response, "ok", false) => (Intrinsic::ResponseOk, vec![receiver]),
            (Kind::Response, "headers", false) => (Intrinsic::ResponseHeaders, vec![receiver]),
            (Kind::Response, "body", false) => (Intrinsic::ResponseBody, vec![receiver]),
            (Kind::Request, "body", false) => (Intrinsic::RequestBody, vec![receiver]),
            (Kind::Response, "bodyUsed", false) => (Intrinsic::ResponseBodyUsed, vec![receiver]),
            (Kind::Response, "bytes", true) => (Intrinsic::ResponseBytes, vec![receiver]),
            (Kind::Response, "text", true) => (Intrinsic::ResponseText, vec![receiver]),
            (Kind::Response, "json", true) => (Intrinsic::ResponseJson, vec![receiver]),
            (Kind::Request, "method", false) => (Intrinsic::RequestMethod, vec![receiver]),
            (Kind::Request, "url", false) => (Intrinsic::RequestUrl, vec![receiver]),
            (Kind::Request, "headers", false) => (Intrinsic::RequestHeaders, vec![receiver]),
            (Kind::Request, "bodyUsed", false) => (Intrinsic::RequestBodyUsed, vec![receiver]),
            (Kind::Request, "bytes", true) => (Intrinsic::RequestBytes, vec![receiver]),
            (Kind::Request, "text", true) => (Intrinsic::RequestText, vec![receiver]),
            (Kind::Request, "json", true) => (Intrinsic::RequestJson, vec![receiver]),
            (Kind::Writable, "locked", false) => (Intrinsic::WritableLocked, vec![receiver]),
            (Kind::Writable, "getWriter", true) => (Intrinsic::WritableGetWriter, vec![receiver]),
            (Kind::Writer, "write", true) => {
                let [chunk] = args.unwrap_or(&[]) else {
                    bail!("write takes one chunk");
                };
                let (chunk, _) = self.body_init(ctx, Some(chunk))?;
                let chunk = self.defined(ctx, chunk)?;
                return Ok(self
                    .intrinsic(ctx, Intrinsic::WriterWrite, vec![receiver, chunk])
                    .expect("promise"));
            }
            (Kind::Writer, "close", true) => (Intrinsic::WriterClose, vec![receiver]),
            (Kind::Readable, "locked", false) => (Intrinsic::ReadableLocked, vec![receiver]),
            (Kind::Readable, "getReader", true) => {
                let args = args.unwrap_or(&[]);
                if args.is_empty() {
                    (Intrinsic::ReadableGetReader, vec![receiver])
                } else {
                    ensure!(
                        args.len() == 1,
                        "getReader takes an optional {{mode: 'byob'}}"
                    );
                    let fields = self.init_fields(args.first())?;
                    ensure!(
                        fields.len() == 1
                            && fields[0].0 == "mode"
                            && matches!(fields[0].1, Expr::String(mode) if mode == "byob"),
                        "getReader supports only {{mode: 'byob'}}"
                    );
                    return Ok(self
                        .intrinsic(ctx, Intrinsic::ReadableGetByobReader, vec![receiver])
                        .expect("reader"));
                }
            }
            (Kind::Readable, "cancel", true) => (Intrinsic::ReadableCancel, vec![receiver]),
            (Kind::Reader, "read", true) => {
                ensure!(
                    args.is_some_and(<[Expr]>::is_empty),
                    "default reader.read takes no arguments"
                );
                (Intrinsic::ReaderRead, vec![receiver])
            }
            (Kind::ByobReader, "read", true) => {
                let args = args.unwrap_or(&[]);
                ensure!(
                    (1..=2).contains(&args.len()),
                    "BYOB read requires a Uint8Array and optional {{min}}"
                );
                let view = self.typed_operand(ctx, &args[0], Kind::Bytes, "a Uint8Array")?;
                let fields = self.init_fields(args.get(1))?;
                ensure!(
                    fields.is_empty() || (fields.len() == 1 && fields[0].0 == "min"),
                    "BYOB read supports only {{min}}"
                );
                let minimum = self.number_or(ctx, fields.first().map(|(_, value)| *value), 1.0)?;
                return Ok(self
                    .intrinsic(
                        ctx,
                        Intrinsic::ReaderReadByob,
                        vec![receiver, view, minimum],
                    )
                    .expect("promise"));
            }
            (Kind::Reader | Kind::ByobReader, "cancel", true) => {
                (Intrinsic::ReaderCancel, vec![receiver])
            }
            (Kind::Reader | Kind::ByobReader, "releaseLock", true) => {
                self.intrinsic(ctx, Intrinsic::ReaderReleaseLock, vec![receiver]);
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (Kind::ReadResult, "done", false) => (Intrinsic::ReadDone, vec![receiver]),
            (Kind::ReadResult, "value", false) => (Intrinsic::ReadValue, vec![receiver]),
            (Kind::Writer, "releaseLock", true) => {
                self.intrinsic(ctx, Intrinsic::WriterReleaseLock, vec![receiver]);
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (_, name, _) => bail!("{name} is not supported on this type yet"),
        };
        if args.is_some_and(|args| !args.is_empty()) && !matches!(receiver_kind, Kind::Headers) {
            bail!("{name} takes no arguments");
        }
        Ok(self.intrinsic(ctx, intrinsic, operands).expect("value"))
    }

    /// A `node:fs/promises` function: an operation whose promise settles from host events.
    fn fs(&mut self, ctx: &mut Context, builtin: Builtin, args: &[Expr]) -> Result<Value> {
        let path = args.first().context("a path is required")?;
        let path = self.typed_operand(ctx, path, Kind::String, "a path string")?;
        let (intrinsic, operands) = match builtin {
            Builtin::ReadFile => {
                let text = match args.get(1) {
                    None => false,
                    Some(Expr::String(encoding)) if is_utf8(encoding) => true,
                    // `{ encoding: 'utf8' }`: an object literal of one field.
                    Some(Expr::New {
                        class_name, args, ..
                    }) if matches!(args.as_slice(), [Expr::String(encoding)] if is_utf8(encoding))
                        && self
                            .shapes
                            .literal_fields(class_name)
                            .is_some_and(|names| names == ["encoding"]) =>
                    {
                        true
                    }
                    Some(_) => bail!("readFile supports no options but the 'utf8' encoding yet"),
                };
                let intrinsic = if text {
                    Intrinsic::FsReadText
                } else {
                    Intrinsic::FsReadBytes
                };
                (intrinsic, vec![path])
            }
            Builtin::WriteFile => {
                ensure!(args.len() == 2, "writeFile options are not supported yet");
                let data = self.expression(ctx, &args[1])?;
                let data = self.defined(ctx, data)?;
                let intrinsic = match kind(ctx, data)? {
                    Kind::String => Intrinsic::FsWriteText,
                    Kind::Bytes => Intrinsic::FsWriteBytes,
                    _ => bail!("writeFile writes strings and Uint8Arrays"),
                };
                (intrinsic, vec![path, data])
            }
            other => {
                ensure!(
                    args.len() == 1,
                    "{other:?} options are not supported yet (recursive, withFileTypes)"
                );
                let intrinsic = match other {
                    Builtin::Stat => Intrinsic::FsStat,
                    Builtin::Readdir => Intrinsic::FsReaddir,
                    Builtin::Mkdir => Intrinsic::FsMkdir,
                    Builtin::Unlink => Intrinsic::FsUnlink,
                    _ => Intrinsic::FsRmdir,
                };
                (intrinsic, vec![path])
            }
        };
        Ok(self.intrinsic(ctx, intrinsic, operands).expect("promise"))
    }

    /// Results of falling off the end or `return;`: an async function settles `undefined`.
    fn implicit_results(&mut self, ctx: &mut Context) -> Vec<Value> {
        self.results
            .clone()
            .into_iter()
            .map(|ty| match optional_value(ctx, ty) {
                Some(inner) if self.is_async => {
                    self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, inner))
                }
                _ => self.emit_value(ctx, |ctx| UndefOp::new(ctx, ty)),
            })
            .collect()
    }

    // ── IR plumbing ────────────────────────────────────────────────────────

    fn append(&mut self, ctx: &mut Context, op: &dyn Op) {
        self.ins.append_op(ctx, op);
        self.guard(ctx, op.get_operation());
    }

    fn append_value<T: Op + OneResultInterface>(&mut self, ctx: &mut Context, op: &T) -> Value {
        self.ins.append_op(ctx, op);
        self.guard(ctx, op.get_operation());
        op.get_result(ctx)
    }

    fn emit<T: Op>(&mut self, ctx: &mut Context, make: impl FnOnce(&mut Context) -> T) -> T {
        let op = make(ctx);
        self.ins.append_op(ctx, &op);
        self.guard(ctx, op.get_operation());
        op
    }

    /// Inside a `try`, route the exception of an op that may throw to the handler.
    fn guard(&mut self, ctx: &mut Context, op: Ptr<Operation>) {
        let Some(handler) = self.handlers.last().copied() else {
            return;
        };
        let throws = may_throw(ctx, op) && Operation::get_op::<RethrowOp>(op, ctx).is_none();
        if !throws {
            return;
        }
        let caught = CaughtOp::new(ctx);
        self.ins.append_op(ctx, &caught);
        let status = caught.get_result(ctx);
        let zero = self.i32(ctx, 0);
        let failed = MirCompare::new(ctx, CompareKind::Ne, status, zero);
        self.ins.append_op(ctx, &failed);
        let failed = failed.get_result(ctx);
        let land = self.new_block(ctx, "throw");
        let next = self.new_block(ctx, "caught");
        let branch = CondBrOp::new(ctx, failed, land, vec![], next, vec![]);
        self.ins.append_op(ctx, &branch);
        self.switch_to(land);
        let store = StoreOp::new(ctx, handler.status, status);
        self.ins.append_op(ctx, &store);
        let br = BrOp::new(ctx, handler.block, vec![]);
        self.ins.append_op(ctx, &br);
        self.switch_to(next);
    }

    fn i32(&mut self, ctx: &mut Context, value: u64) -> Value {
        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(value, bw(32)));
        let op = ConstantOp::new(ctx, Box::new(attr));
        self.ins.append_op(ctx, &op);
        op.get_result(ctx)
    }

    /// Leave with exception `status`: to the innermost handler, or out of the function. Ends
    /// the current block.
    fn propagate(&mut self, ctx: &mut Context, status: Value) {
        match self.handlers.last().copied() {
            Some(handler) => {
                self.store(ctx, handler.status, status);
                self.emit(ctx, |ctx| BrOp::new(ctx, handler.block, vec![]));
            }
            None => {
                self.emit(ctx, |ctx| RethrowOp::new(ctx, status));
            }
        }
    }

    /// Run the bodies of the `finally` blocks that a jump out of them leaves, innermost first,
    /// with only the handlers outside each `try` active.
    fn run_finallies(
        &mut self,
        ctx: &mut Context,
        leaving: impl Fn(&Finally) -> bool,
    ) -> Result<()> {
        let pending: Vec<(usize, Finally)> = self
            .finallies
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, f)| leaving(f))
            .collect();
        for (index, finally) in pending.into_iter().rev() {
            let handlers = self.handlers.split_off(finally.handlers);
            let finallies = self.finallies.split_off(index);
            let lowered = self.statements(ctx, &finally.body);
            self.handlers.extend(handlers);
            self.finallies.extend(finallies);
            lowered?;
            if self.terminated(ctx) {
                break;
            }
        }
        Ok(())
    }

    /// `try { body } catch (param) { handler } finally { cleanup }`.
    fn try_(
        &mut self,
        ctx: &mut Context,
        body: &[Stmt],
        catch: Option<&perry_hir::ir::CatchClause>,
        finally: Option<&[Stmt]>,
    ) -> Result<()> {
        let i32 = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let status = self.entry_slot(ctx, i32);
        let join = self.new_block(ctx, "join");
        let outer = Finally {
            body: finally.map(<[Stmt]>::to_vec).unwrap_or_default(),
            handlers: self.handlers.len(),
            controls: self.controls.len(),
        };
        // The exceptional path of `finally`: run it, then throw the exception again.
        let rethrow_block = finally.map(|_| self.new_block(ctx, "finally"));
        let catch_block = catch.map(|_| self.new_block(ctx, "catch"));
        let first = catch_block
            .or(rethrow_block)
            .context("try without catch or finally")?;

        if finally.is_some() {
            self.finallies.push(outer.clone());
        }
        self.handlers.push(Handler {
            block: first,
            status,
        });
        let lowered = self.statements(ctx, body);
        self.handlers.pop();
        if finally.is_some() {
            self.finallies.pop();
        }
        lowered?;
        if !self.terminated(ctx) {
            self.statements(ctx, &outer.body)?;
            self.jump(ctx, join);
        }

        if let (Some(clause), Some(block)) = (catch, catch_block) {
            self.switch_to(block);
            let caught = self.load(ctx, status)?;
            self.pass_exit(ctx, caught);
            let exception = self.emit_value(ctx, |ctx| CatchOp::new(ctx, caught));
            if let Some((id, _)) = &clause.param {
                let ty = exception.get_type(ctx);
                let slot = self.entry_slot(ctx, ty);
                self.store(ctx, slot, exception);
                self.locals.insert(*id, slot);
            }
            if let Some(rethrow) = rethrow_block {
                self.finallies.push(outer.clone());
                self.handlers.push(Handler {
                    block: rethrow,
                    status,
                });
            }
            let lowered = self.statements(ctx, &clause.body);
            if rethrow_block.is_some() {
                self.handlers.pop();
                self.finallies.pop();
            }
            lowered?;
            if !self.terminated(ctx) {
                self.statements(ctx, &outer.body)?;
                self.jump(ctx, join);
            }
        }

        if let Some(block) = rethrow_block {
            self.switch_to(block);
            let caught = self.load(ctx, status)?;
            self.pass_exit(ctx, caught);
            // Hold the exception across the cleanup, which may throw and catch its own.
            let exception = self.emit_value(ctx, |ctx| CatchOp::new(ctx, caught));
            self.statements(ctx, &outer.body)?;
            if !self.terminated(ctx) {
                let again = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, exception));
                self.propagate(ctx, again);
            }
        }
        self.switch_to(join);
        Ok(())
    }

    /// `process.exit` unwinds through handlers without running them.
    fn pass_exit(&mut self, ctx: &mut Context, status: Value) {
        let exit = self.i32(ctx, EXIT_STATUS);
        let exiting = self.compare(ctx, CompareKind::Eq, status, exit);
        let leave = self.new_block(ctx, "exit");
        let stay = self.new_block(ctx, "handle");
        self.branch(ctx, exiting, leave, stay);
        self.switch_to(leave);
        self.propagate(ctx, status);
        self.switch_to(stay);
    }

    fn emit_value<T: Op + OneResultInterface>(
        &mut self,
        ctx: &mut Context,
        make: impl FnOnce(&mut Context) -> T,
    ) -> Value {
        let op = self.emit(ctx, make);
        op.get_result(ctx)
    }

    fn new_block(&mut self, ctx: &mut Context, label: &str) -> Ptr<BasicBlock> {
        self.labels += 1;
        let name = Identifier::try_from(format!("{label}{}", self.labels)).expect("label");
        let block = BasicBlock::new(ctx, Some(name), vec![]);
        block.insert_at_back(self.region, ctx);
        block
    }

    fn switch_to(&mut self, block: Ptr<BasicBlock>) {
        self.block = block;
        self.ins.set_insertion_point_to_block_end(block);
    }

    fn terminated(&self, ctx: &Context) -> bool {
        self.block.deref(ctx).get_terminator(ctx).is_some()
    }

    /// Branch to `target` unless the current block already ended (e.g. with `return`).
    fn jump(&mut self, ctx: &mut Context, target: Ptr<BasicBlock>) {
        if !self.terminated(ctx) {
            self.emit(ctx, |ctx| BrOp::new(ctx, target, vec![]));
        }
    }

    /// After `return`/`break`/`continue`, keep lowering into an unreachable block.
    fn after_terminator(&mut self, ctx: &mut Context) {
        let dead = self.new_block(ctx, "unreachable");
        self.switch_to(dead);
    }

    fn slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        self.emit_value(ctx, |ctx| LocalOp::new(ctx, ty))
    }

    fn store(&mut self, ctx: &mut Context, slot: Value, value: Value) {
        self.emit(ctx, |ctx| StoreOp::new(ctx, slot, value));
    }

    fn load(&mut self, ctx: &mut Context, slot: Value) -> Result<Value> {
        if self.global_slots.contains(&slot) {
            let op = perry_mir::globals::GlobalLoadOp::new(ctx, slot)?;
            return Ok(self.append_value(ctx, &op));
        }
        let op = LoadOp::new(ctx, slot)?;
        Ok(self.append_value(ctx, &op))
    }

    fn local(&self, id: LocalId) -> Result<Value> {
        self.locals.get(&id).copied().ok_or_else(|| {
            anyhow!(
                "unsupported reference to local {id} (globals and captures are not supported yet)"
            )
        })
    }

    fn number(&mut self, ctx: &mut Context, value: f64) -> Value {
        let op = ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(value)));
        self.append_value(ctx, &op)
    }

    fn boolean(&mut self, ctx: &mut Context, value: bool) -> Value {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(u64::from(value), bw(1)));
        let op = ConstantOp::new(ctx, Box::new(attr));
        self.append_value(ctx, &op)
    }

    fn binary(&mut self, ctx: &mut Context, kind: BinaryKind, lhs: Value, rhs: Value) -> Value {
        self.emit_value(ctx, |ctx| MirBinary::new(ctx, kind, lhs, rhs))
    }

    fn compare(&mut self, ctx: &mut Context, kind: CompareKind, lhs: Value, rhs: Value) -> Value {
        self.emit_value(ctx, |ctx| MirCompare::new(ctx, kind, lhs, rhs))
    }

    fn condition(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        self.truthy(ctx, value)
    }

    fn truthy(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if self.is_optional(ctx, value) {
            let absent = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            let ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
            return self.choose(
                ctx,
                ty,
                absent,
                |b, ctx| Ok(b.boolean(ctx, false)),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                    b.choose(
                        ctx,
                        ty,
                        null,
                        |b, ctx| Ok(b.boolean(ctx, false)),
                        |b, ctx| {
                            let value = b.defined(ctx, value)?;
                            b.truthy(ctx, value)
                        },
                    )
                },
            );
        }
        match kind(ctx, value)? {
            Kind::Boolean => Ok(value),
            Kind::String => {
                let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, value));
                let zero = self.i32(ctx, 0);
                Ok(self.compare(ctx, CompareKind::Ne, length, zero))
            }
            Kind::Number => {
                let zero = self.number(ctx, 0.0);
                let absolute = self
                    .intrinsic(ctx, Intrinsic::MathAbs, vec![value])
                    .expect("number");
                Ok(self.compare(ctx, CompareKind::Gt, absolute, zero))
            }
            Kind::Json => Ok(self
                .intrinsic(ctx, Intrinsic::JsonTruthy, vec![value])
                .expect("boolean")),
            Kind::BigInt | Kind::NumberString => {
                bail!("narrow this scalar before testing truthiness")
            }
            _ => Ok(self.boolean(ctx, true)),
        }
    }

    fn branch(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: Ptr<BasicBlock>,
        otherwise: Ptr<BasicBlock>,
    ) {
        self.emit(ctx, |ctx| {
            CondBrOp::new(ctx, condition, then, vec![], otherwise, vec![])
        });
    }

    // ── Statements ─────────────────────────────────────────────────────────

    fn statements(&mut self, ctx: &mut Context, statements: &[Stmt]) -> Result<()> {
        let mut index = 0;
        while index < statements.len() {
            if let Some(element) = self.destructured_element(&statements[index..]) {
                self.statement(ctx, &element)?;
                index += 2;
                continue;
            }
            self.statement(ctx, &statements[index])?;
            index += 1;
        }
        Ok(())
    }

    /// Array destructuring declares each element as `let e = undefined` and assigns it in
    /// `if (guard) { <iterator pull> } else { e = i < a.length ? a[i] : undefined }`. With a
    /// statically false guard this is `let e = a[i]` (a read that is `undefined` out of
    /// bounds, R8).
    fn destructured_element(&self, statements: &[Stmt]) -> Option<Stmt> {
        let [
            Stmt::Let {
                id,
                name,
                ty: HirType::Any,
                init: Some(Expr::Undefined),
                ..
            },
            Stmt::If {
                condition: Expr::LocalGet(guard),
                else_branch: Some(else_branch),
                ..
            },
            ..,
        ] = statements
        else {
            return None;
        };
        let [Stmt::Expr(Expr::LocalSet(target, value))] = else_branch.as_slice() else {
            return None;
        };
        if target != id || !self.false_guards.contains(guard) {
            return None;
        }
        let value = match &**value {
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } if matches!(**else_expr, Expr::Undefined)
                && matches!(**then_expr, Expr::IndexGet { .. }) =>
            {
                (**then_expr).clone()
            }
            other => other.clone(),
        };
        Some(Stmt::Let {
            id: *id,
            name: name.clone(),
            ty: HirType::Any,
            mutable: false,
            init: Some(value),
        })
    }

    fn statement(&mut self, ctx: &mut Context, statement: &Stmt) -> Result<()> {
        match statement {
            Stmt::Let {
                id,
                init: Some(guard),
                ..
            } if self.iterator_guard(ctx, guard)? => {
                self.false_guards.insert(*id);
            }
            // The iterator of the protocol arm (`guard ? GetIterator(a) : undefined`).
            Stmt::Let {
                init: Some(Expr::Conditional { condition, .. }),
                ..
            } if matches!(condition.as_ref(), Expr::LocalGet(guard) if self.false_guards.contains(guard)) =>
                {}
            Stmt::If {
                condition: Expr::LocalGet(guard),
                else_branch,
                ..
            } if self.false_guards.contains(guard) => {
                if let Some(else_branch) = else_branch {
                    self.statements(ctx, else_branch)?;
                }
            }
            Stmt::Let { id, ty, init, .. } => {
                // Perry HIR leaves some bindings untyped (`Any`); those take the initializer's
                // type. An explicit type the subset cannot represent is an error, not a fallback.
                // `Array(Any)` is such a binding too (array destructuring's element source).
                let declared = match ty {
                    HirType::Any | HirType::Unknown => None,
                    HirType::Array(element) if matches!(**element, HirType::Any) => None,
                    ty => Some(self.shapes.value_type(ctx, ty)?),
                };
                let init = init
                    .as_ref()
                    .map(|e| self.expression_with(ctx, e, declared))
                    .transpose()?;
                // Inferred bindings preserve absence states and the selected reader mode.
                // HIR's getReader signature assumes the default overload.
                let ty = match (declared, init) {
                    (Some(ty), Some(value))
                        if self.shapes.inferred_bindings.contains(id)
                            && (optional_value(ctx, value.get_type(ctx)) == Some(ty)
                                || (ty.deref(ctx).is::<ReaderType>()
                                    && kind(ctx, value)? == Kind::ByobReader)) =>
                    {
                        value.get_type(ctx)
                    }
                    (Some(ty), _) => ty,
                    (None, Some(value)) => value.get_type(ctx),
                    (None, None) => bail!("an untyped binding needs an initializer"),
                };
                let slot = if self.shapes.global_ids.contains(id) {
                    self.shapes.globals.borrow_mut().insert(*id, ty);
                    self.emit_value(ctx, |ctx| {
                        perry_mir::globals::GlobalOp::new(
                            ctx,
                            format!("global_{id}").try_into().expect("global name"),
                            ty,
                        )
                    })
                } else {
                    self.entry_slot(ctx, ty)
                };
                if self.shapes.global_ids.contains(id) {
                    self.global_slots.insert(slot);
                }
                if let Some(value) = init {
                    let value = self.fit(ctx, value, ty)?;
                    self.store(ctx, slot, value);
                }
                self.locals.insert(*id, slot);
            }
            Stmt::Expr(expr) => {
                self.effect(ctx, expr)?;
            }
            Stmt::Return(value) => {
                let mut adopts_promise = false;
                let mut values = match value {
                    Some(expr) => {
                        let expected = self
                            .results
                            .first()
                            .copied()
                            .context("a void function returns a value")?;
                        let value = self.expression_with(ctx, expr, Some(expected))?;
                        adopts_promise =
                            self.is_async && value.get_type(ctx).deref(ctx).is::<PromiseType>();
                        vec![if adopts_promise {
                            value
                        } else {
                            self.fit(ctx, value, expected)?
                        }]
                    }
                    None => self.implicit_results(ctx),
                };
                self.run_finallies(ctx, |_| true)?;
                if self.terminated(ctx) {
                    self.after_terminator(ctx);
                    return Ok(());
                }
                if adopts_promise {
                    // Returning a promise runs finalizers before adoption; its later rejection
                    // does not enter a catch surrounding the return expression.
                    let handlers = std::mem::take(&mut self.handlers);
                    let await_ = AwaitOp::new(ctx, values[0])?;
                    let value = self.append_value(ctx, &await_);
                    values[0] = self.fit(ctx, value, self.results[0])?;
                    self.handlers = handlers;
                }
                ensure!(
                    values.iter().map(|v| v.get_type(ctx)).collect::<Vec<_>>() == self.results,
                    "return value type differs from the declared result"
                );
                self.emit(ctx, |ctx| ReturnOp::new(ctx, values));
                self.after_terminator(ctx);
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let guards = self.variant_guards.clone();
                let test = self.condition(ctx, condition)?;
                let then = self.new_block(ctx, "then");
                let join = self.new_block(ctx, "join");
                let otherwise = if else_branch.is_some() {
                    self.new_block(ctx, "else")
                } else {
                    join
                };
                self.branch(ctx, test, then, otherwise);
                self.switch_to(then);
                self.variant_condition(condition, true);
                self.statements(ctx, then_branch)?;
                let then_reachable = self.reachable(ctx, self.block);
                self.jump(ctx, join);
                self.variant_guards = guards.clone();
                let mut else_reachable = true;
                if let Some(else_branch) = else_branch {
                    self.switch_to(otherwise);
                    self.variant_condition(condition, false);
                    self.statements(ctx, else_branch)?;
                    else_reachable = self.reachable(ctx, self.block);
                    self.jump(ctx, join);
                }
                self.variant_guards = guards;
                if !then_reachable {
                    self.variant_condition(condition, false);
                }
                if !else_reachable {
                    self.variant_condition(condition, true);
                }
                self.switch_to(join);
            }
            Stmt::While { condition, body } => {
                self.loop_(ctx, None, Some(condition), None, body, false)?
            }
            Stmt::DoWhile { body, condition } => {
                self.loop_(ctx, None, Some(condition), None, body, true)?
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => self.loop_(
                ctx,
                init.as_deref(),
                condition.as_ref(),
                update.as_ref(),
                body,
                false,
            )?,
            Stmt::Switch {
                discriminant,
                cases,
            } => self.switch(ctx, discriminant, cases)?,
            Stmt::Break | Stmt::Continue => {
                let (index, target) = self
                    .controls
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, control)| match (statement, control) {
                        (
                            Stmt::Break,
                            Control::Loop { break_to, .. } | Control::Switch { break_to },
                        ) => Some((index, *break_to)),
                        (Stmt::Continue, Control::Loop { continue_to, .. }) => {
                            Some((index, *continue_to))
                        }
                        _ => None,
                    })
                    .context("break/continue has no enclosing target")?;
                self.run_finallies(ctx, |f| f.controls > index)?;
                self.jump(ctx, target);
                self.after_terminator(ctx);
            }
            Stmt::Throw(value) => {
                let value = self.expression(ctx, value)?;
                let value = self.defined(ctx, value)?;
                ensure!(
                    matches!(
                        kind(ctx, value)?,
                        Kind::Exception
                            | Kind::Number
                            | Kind::Boolean
                            | Kind::String
                            | Kind::BigInt
                    ),
                    "only Error objects, numbers, bigints, booleans and strings can be thrown"
                );
                let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, value));
                self.propagate(ctx, status);
                self.after_terminator(ctx);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => self.try_(ctx, body, catch.as_ref(), finally.as_deref())?,
            other => bail!(
                "MIR backend does not support statement {} yet",
                statement_name(other)
            ),
        }
        Ok(())
    }

    /// `for (init; condition; update) body`, `while`, and `do … while`.
    fn loop_(
        &mut self,
        ctx: &mut Context,
        init: Option<&Stmt>,
        condition: Option<&Expr>,
        update: Option<&Expr>,
        body: &[Stmt],
        body_first: bool,
    ) -> Result<()> {
        if let Some(init) = init {
            self.statement(ctx, init)?;
        }
        let header = self.new_block(ctx, "header");
        let body_block = self.new_block(ctx, "body");
        let latch = self.new_block(ctx, "latch");
        let exit = self.new_block(ctx, "exit");
        self.jump(ctx, if body_first { body_block } else { header });

        self.switch_to(header);
        match condition {
            Some(condition) => {
                let condition = self.condition(ctx, condition)?;
                self.branch(ctx, condition, body_block, exit);
            }
            None => self.jump(ctx, body_block),
        }

        self.switch_to(body_block);
        self.controls.push(Control::Loop {
            continue_to: latch,
            break_to: exit,
        });
        let lowered = self.statements(ctx, body);
        self.controls.pop();
        lowered?;
        self.jump(ctx, latch);

        self.switch_to(latch);
        if let Some(update) = update {
            self.effect(ctx, update)?;
        }
        self.jump(ctx, header);
        self.switch_to(exit);
        Ok(())
    }

    /// Lower an expression evaluated for its effects; void calls are allowed here.
    fn effect(&mut self, ctx: &mut Context, expr: &Expr) -> Result<()> {
        if let Expr::Call { callee, args, .. } = expr {
            self.call(ctx, callee, args)?;
        } else if let Expr::ProcessExit(code) = expr {
            let code = match code {
                Some(code) => {
                    let code = self.expression(ctx, code)?;
                    let code = self.arithmetic(ctx, code);
                    ensure!(
                        kind(ctx, code)? == Kind::Number,
                        "process.exit takes a number"
                    );
                    code
                }
                None => self.number(ctx, 0.0),
            };
            let status = self.emit_value(ctx, |ctx| ExitOp::new(ctx, code));
            self.propagate(ctx, status);
            self.after_terminator(ctx);
        } else {
            self.expression(ctx, expr)?;
        }
        Ok(())
    }

    // ── Expressions ────────────────────────────────────────────────────────

    fn expression(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        self.expression_with(ctx, expr, None)
    }

    /// Lower `expr`; `expected` types literals whose own type is not determined by their
    /// operands (empty arrays, object literals widened to a declared shape).
    fn expression_with(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        if let Some(value) = self.date_expression(ctx, expr)? {
            return Ok(value);
        }
        if let Some(value) = self.url_expression(ctx, expr)? {
            return Ok(value);
        }
        let value = match expr {
            Expr::RegExp { pattern, flags } => self.emit_value(ctx, |ctx| {
                perry_mir::regex::RegexConstOp::new(ctx, pattern, flags)
            }),
            Expr::RegExpDynamic { .. } => {
                bail!("RegExp construction is unsupported; use a bounded regex literal")
            }
            Expr::RegExpTest { regex, string } => {
                let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                let string = self.string_operand(ctx, string)?;
                self.regex_test(ctx, regex, string)
            }
            Expr::BigInt(text) => {
                let text = text.trim_end_matches('n').replace('_', "");
                let (radix, digits) = if let Some(n) =
                    text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
                {
                    (16, n)
                } else if let Some(n) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O"))
                {
                    (8, n)
                } else if let Some(n) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B"))
                {
                    (2, n)
                } else {
                    (10, text.as_str())
                };
                let magnitude = u64::from_str_radix(digits, radix)
                    .context("bigint literal exceeds the 64-bit transport domain")?;
                self.emit_value(ctx, |ctx| {
                    perry_mir::integers::BigIntConstOp::new(ctx, magnitude)
                })
            }
            Expr::BigIntCoerce(expr) => {
                let value = self.expression(ctx, expr)?;
                let value = self.defined(ctx, value)?;
                match kind(ctx, value)? {
                    Kind::BigInt => value,
                    Kind::Number => self
                        .intrinsic(ctx, Intrinsic::BigIntFromNumber, vec![value])
                        .expect("bigint"),
                    _ => bail!(
                        "BigInt conversion requires a number or bigint in the 64-bit transport profile"
                    ),
                }
            }
            Expr::NumberCoerce(expr) => {
                let value = self.expression(ctx, expr)?;
                ensure!(
                    kind(ctx, value)? != Kind::NumberString,
                    "narrow number | string before numeric conversion"
                );
                if kind(ctx, value)? == Kind::BigInt {
                    self.intrinsic(ctx, Intrinsic::BigIntToNumber, vec![value])
                        .expect("number")
                } else {
                    self.arithmetic(ctx, value)
                }
            }
            Expr::StringCoerce(expr) | Expr::TemplateStringCoerce(expr) => {
                let value = self.expression(ctx, expr)?;
                self.display(ctx, value)?
            }
            Expr::Number(value) => self.number(ctx, *value),
            Expr::Integer(value) => self.number(ctx, *value as f64),
            Expr::Bool(value) => self.boolean(ctx, *value),
            Expr::String(text) => {
                let op = StringConstOp::new(ctx, text);
                self.append_value(ctx, &op)
            }
            Expr::LocalGet(id) => {
                let slot = self.local(*id)?;
                self.load(ctx, slot)?
            }
            Expr::Null | Expr::Undefined => {
                let null = matches!(expr, Expr::Null);
                let ty = match expected {
                    Some(ty) => ty,
                    None => {
                        let number = FP64Type::get(ctx).into();
                        let kind = if null {
                            perry_mir::attrs::Absence::Null
                        } else {
                            perry_mir::attrs::Absence::Undefined
                        };
                        perry_mir::types::OptionalType::get_with(ctx, number, kind).into()
                    }
                };
                let allowed = absence(ctx, ty).is_some_and(|a| {
                    if null {
                        a.admits_null()
                    } else {
                        a.admits_undefined()
                    }
                });
                ensure!(
                    allowed,
                    "{} is not admitted by the declared type",
                    if null { "null" } else { "undefined" }
                );
                if null {
                    self.emit_value(ctx, |ctx| NullOp::new(ctx, ty))
                } else {
                    self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
                }
            }
            Expr::LocalSet(id, value) => {
                let slot = self.local(*id)?;
                let slot_type = self.slot_type(ctx, slot)?;
                let value = self.expression_with(ctx, value, Some(slot_type))?;
                let value = self.fit(ctx, value, slot_type)?;
                self.store(ctx, slot, value);
                value
            }
            Expr::Update { id, op, prefix } => {
                let slot = self.local(*id)?;
                let old = self.load(ctx, slot)?;
                let old = self.arithmetic(ctx, old);
                ensure!(kind(ctx, old)? == Kind::Number, "++/-- needs a number");
                let one = self.number(ctx, 1.0);
                let kind = match op {
                    UpdateOp::Increment => BinaryKind::Add,
                    UpdateOp::Decrement => BinaryKind::Sub,
                };
                let new = self.binary(ctx, kind, old, one);
                let slot_type = self.slot_type(ctx, slot)?;
                let stored = self.fit(ctx, new, slot_type)?;
                self.store(ctx, slot, stored);
                if *prefix { new } else { old }
            }
            Expr::PropertyUpdate {
                object,
                property,
                op,
                prefix,
                ..
            } => {
                let object = self.expression(ctx, object)?;
                let object = self.defined(ctx, object)?;
                let field = property.clone();
                let get = FieldGetOp::new(ctx, object, field.clone())
                    .map_err(|_| anyhow!("property update needs a declared numeric field"))?;
                let previous = self.append_value(ctx, &get);
                let field_type = previous.get_type(ctx);
                let old = self.arithmetic(ctx, previous);
                ensure!(
                    kind(ctx, old)? == Kind::Number,
                    "property update needs a numeric field"
                );
                let one = self.number(ctx, 1.0);
                let kind = match op {
                    BinaryOp::Add => BinaryKind::Add,
                    BinaryOp::Sub => BinaryKind::Sub,
                    _ => bail!("invalid property update"),
                };
                let next = self.binary(ctx, kind, old, one);
                let stored = self.fit(ctx, next, field_type)?;
                self.emit(ctx, |ctx| FieldSetOp::new(ctx, object, field, stored));
                if *prefix { next } else { old }
            }
            Expr::Binary { op, left, right } => {
                let lhs = self.expression(ctx, left)?;
                let lhs = self.arithmetic(ctx, lhs);
                let rhs = self.expression(ctx, right)?;
                let rhs = self.arithmetic(ctx, rhs);
                match (op, kind(ctx, lhs)?, kind(ctx, rhs)?) {
                    (BinaryOp::Add, Kind::String, Kind::String) => {
                        self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, lhs, rhs))
                    }
                    (BinaryOp::Add, Kind::String, _) | (BinaryOp::Add, _, Kind::String) => {
                        let lhs = self.display(ctx, lhs)?;
                        let rhs = self.display(ctx, rhs)?;
                        self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, lhs, rhs))
                    }
                    (op, Kind::Number, Kind::Number) => {
                        let kind = match op {
                            BinaryOp::Add => BinaryKind::Add,
                            BinaryOp::Sub => BinaryKind::Sub,
                            BinaryOp::Mul => BinaryKind::Mul,
                            BinaryOp::Div => BinaryKind::Div,
                            BinaryOp::Mod => BinaryKind::Rem,
                            other => bail!("MIR backend does not support operator {other:?} yet"),
                        };
                        self.binary(ctx, kind, lhs, rhs)
                    }
                    (op, _, _) => {
                        bail!("operator {op:?} on these operand types is not supported yet")
                    }
                }
            }
            Expr::Compare { op, left, right } => {
                if matches!(op, CompareOp::Eq | CompareOp::Ne)
                    && matches!(&**left, Expr::Null | Expr::Undefined)
                    && matches!(&**right, Expr::Null | Expr::Undefined)
                {
                    let equal = matches!(&**left, Expr::Null) == matches!(&**right, Expr::Null);
                    return Ok(self.boolean(ctx, equal == matches!(op, CompareOp::Eq)));
                }
                if let (CompareOp::Eq | CompareOp::Ne, Some(other)) = (
                    op,
                    match (&**left, &**right) {
                        (Expr::Undefined | Expr::Null, other)
                        | (other, Expr::Undefined | Expr::Null) => Some(other),
                        _ => None,
                    },
                ) {
                    let value = self.expression(ctx, other)?;
                    let undefined = if let Some(json) = self.json_operand(ctx, value)? {
                        let intrinsic =
                            if matches!(&**left, Expr::Null) || matches!(&**right, Expr::Null) {
                                Intrinsic::JsonIsNull
                            } else {
                                Intrinsic::JsonIsUndefined
                            };
                        self.intrinsic(ctx, intrinsic, vec![json]).expect("boolean")
                    } else if self.is_optional(ctx, value) {
                        if matches!(&**left, Expr::Null) || matches!(&**right, Expr::Null) {
                            self.emit_value(ctx, |ctx| IsNullOp::new(ctx, value))
                        } else {
                            self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value))
                        }
                    } else {
                        self.boolean(ctx, false)
                    };
                    return Ok(if matches!(op, CompareOp::Eq) {
                        undefined
                    } else {
                        let no = self.boolean(ctx, false);
                        self.compare(ctx, CompareKind::Eq, undefined, no)
                    });
                }
                let lhs = self.expression(ctx, left)?;
                let rhs = self.expression(ctx, right)?;
                if matches!(op, CompareOp::Eq | CompareOp::Ne) {
                    let json_left = self.json_operand(ctx, lhs)?;
                    let json_right = self.json_operand(ctx, rhs)?;
                    let operands = match (json_left, json_right) {
                        (Some(_), Some(_)) => {
                            bail!(
                                "strict equality between JSON snapshots requires an explicit scalar conversion"
                            )
                        }
                        (Some(json), None) => Some((json, rhs)),
                        (None, Some(json)) => Some((json, lhs)),
                        (None, None) => None,
                    };
                    if let Some((json, scalar)) = operands {
                        let equal = self.json_scalar_equal(ctx, json, scalar)?;
                        return Ok(if matches!(op, CompareOp::Eq) {
                            equal
                        } else {
                            let no = self.boolean(ctx, false);
                            self.compare(ctx, CompareKind::Eq, equal, no)
                        });
                    }
                }
                let union = |value: Value| {
                    optional_value(ctx, value.get_type(ctx))
                        .unwrap_or(value.get_type(ctx))
                        .deref(ctx)
                        .is::<perry_mir::types::NumberStringType>()
                };
                if matches!(op, CompareOp::Eq | CompareOp::Ne) && (union(lhs) || union(rhs)) {
                    let base = perry_mir::types::NumberStringType::get(ctx).into();
                    let states = match (
                        absence(ctx, lhs.get_type(ctx)),
                        absence(ctx, rhs.get_type(ctx)),
                    ) {
                        (Some(a), Some(b)) if a != b => Some(perry_mir::attrs::Absence::Both),
                        (Some(a), _) | (_, Some(a)) => Some(a),
                        _ => None,
                    };
                    let target = states.map_or(base, |a| {
                        perry_mir::types::OptionalType::get_with(ctx, base, a).into()
                    });
                    let lhs = self.fit(ctx, lhs, target)?;
                    let rhs = self.fit(ctx, rhs, target)?;
                    return Ok(self.compare(
                        ctx,
                        if matches!(op, CompareOp::Eq) {
                            CompareKind::Eq
                        } else {
                            CompareKind::Ne
                        },
                        lhs,
                        rhs,
                    ));
                }
                if matches!(op, CompareOp::Eq | CompareOp::Ne)
                    && (self.is_optional(ctx, lhs) || self.is_optional(ctx, rhs))
                {
                    // Strict equality with `undefined` in play: compare as `T | undefined`.
                    let kind = if matches!(op, CompareOp::Eq) {
                        CompareKind::Eq
                    } else {
                        CompareKind::Ne
                    };
                    let left_inner =
                        optional_value(ctx, lhs.get_type(ctx)).unwrap_or(lhs.get_type(ctx));
                    let right_inner =
                        optional_value(ctx, rhs.get_type(ctx)).unwrap_or(rhs.get_type(ctx));
                    ensure!(
                        left_inner == right_inner,
                        "comparison of different types is not supported"
                    );
                    let left_absence = absence(ctx, lhs.get_type(ctx));
                    let right_absence = absence(ctx, rhs.get_type(ctx));
                    let states = match (left_absence, right_absence) {
                        (Some(a), Some(b)) if a != b => perry_mir::attrs::Absence::Both,
                        (Some(a), _) | (_, Some(a)) => a,
                        _ => unreachable!(),
                    };
                    let target =
                        perry_mir::types::OptionalType::get_with(ctx, left_inner, states).into();
                    let lhs = self.fit(ctx, lhs, target)?;
                    let rhs = self.fit(ctx, rhs, target)?;
                    ensure!(
                        lhs.get_type(ctx) == rhs.get_type(ctx),
                        "comparison of different types is not supported"
                    );
                    return Ok(self.compare(ctx, kind, lhs, rhs));
                }
                let lhs = self.arithmetic(ctx, lhs);
                let rhs = self.arithmetic(ctx, rhs);
                let (left_kind, right_kind) = (kind(ctx, lhs)?, kind(ctx, rhs)?);
                ensure!(
                    left_kind != Kind::Regex && right_kind != Kind::Regex,
                    "regex object identity is outside the literal matching profile"
                );
                ensure!(
                    left_kind == right_kind,
                    "comparison of different types is not supported"
                );
                let kind = match op {
                    CompareOp::Eq => CompareKind::Eq,
                    CompareOp::Ne => CompareKind::Ne,
                    CompareOp::Lt => CompareKind::Lt,
                    CompareOp::Le => CompareKind::Le,
                    CompareOp::Gt => CompareKind::Gt,
                    CompareOp::Ge => CompareKind::Ge,
                    CompareOp::LooseEq | CompareOp::LooseNe => {
                        bail!("loose equality is not supported; use === or !==")
                    }
                };
                ensure!(
                    matches!(left_kind, Kind::Number | Kind::String | Kind::BigInt)
                        || matches!(kind, CompareKind::Eq | CompareKind::Ne),
                    "ordered comparison is only supported on numbers and strings"
                );
                self.compare(ctx, kind, lhs, rhs)
            }
            Expr::Logical { op, left, right } => {
                // Short-circuit through a result slot; mem2reg turns it into a block argument.
                ensure!(
                    !matches!(op, LogicalOp::Coalesce),
                    "?? is not supported yet"
                );
                let lhs = self.condition(ctx, left)?;
                let result = self.entry_slot(ctx, lhs.get_type(ctx));
                self.store(ctx, result, lhs);
                let rhs_block = self.new_block(ctx, "rhs");
                let join = self.new_block(ctx, "join");
                match op {
                    LogicalOp::And => self.branch(ctx, lhs, rhs_block, join),
                    _ => self.branch(ctx, lhs, join, rhs_block),
                }
                self.switch_to(rhs_block);
                let rhs = self.condition(ctx, right)?;
                self.store(ctx, result, rhs);
                self.jump(ctx, join);
                self.switch_to(join);
                self.load(ctx, result)?
            }
            Expr::Conditional {
                condition,
                else_expr,
                ..
            } if matches!(&**condition, Expr::LocalGet(guard) if self.false_guards.contains(guard)) => {
                self.expression(ctx, else_expr)?
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => {
                let guards = self.variant_guards.clone();
                let test = self.condition(ctx, condition)?;
                let then = self.new_block(ctx, "then");
                let otherwise = self.new_block(ctx, "else");
                let join = self.new_block(ctx, "join");
                self.branch(ctx, test, then, otherwise);
                self.switch_to(then);
                // With an expected type both arms fit it (`cond ? value : undefined`).
                self.variant_condition(condition, true);
                let then_value = self.expression_with(ctx, then_expr, expected)?;
                let then_value = match expected {
                    Some(expected) => self.fit(ctx, then_value, expected)?,
                    None => then_value,
                };
                let ty = then_value.get_type(ctx);
                // The slot is created in the entry block so it dominates both arms.
                let result = self.entry_slot(ctx, ty);
                self.store(ctx, result, then_value);
                self.jump(ctx, join);
                self.switch_to(otherwise);
                self.variant_guards = guards.clone();
                self.variant_condition(condition, false);
                let else_value = self.expression_with(ctx, else_expr, Some(ty))?;
                let else_value = self.fit(ctx, else_value, ty)?;
                self.store(ctx, result, else_value);
                self.jump(ctx, join);
                self.variant_guards = guards;
                self.switch_to(join);
                self.load(ctx, result)?
            }
            Expr::Unary { op, operand } => {
                let value = self.expression(ctx, operand)?;
                if *op == UnaryOp::Not {
                    let value = self.truthy(ctx, value)?;
                    let no = self.boolean(ctx, false);
                    return Ok(self.compare(ctx, CompareKind::Eq, value, no));
                }
                let value = self.arithmetic(ctx, value);
                match (op, kind(ctx, value)?) {
                    (UnaryOp::Neg, Kind::BigInt) => self
                        .intrinsic(ctx, Intrinsic::BigIntNegate, vec![value])
                        .expect("bigint"),
                    (UnaryOp::Neg, Kind::Number) => {
                        // `0 - x` loses the sign of zero; `-1 * x` keeps IEEE negation.
                        let minus_one = self.number(ctx, -1.0);
                        self.binary(ctx, BinaryKind::Mul, minus_one, value)
                    }
                    (UnaryOp::Pos, Kind::Number) => value,
                    (op, _) => bail!("unary {op:?} on this type is not supported yet"),
                }
            }
            Expr::StringSplit(text, separator) => {
                let text = self.string_operand(ctx, text)?;
                let separator = self.string_operand(ctx, separator)?;
                self.intrinsic(ctx, Intrinsic::StringSplit, vec![text, separator])
                    .expect("strings")
            }
            Expr::Call { callee, args, .. } => self
                .call(ctx, callee, args)?
                .context("a void call has no value")?,
            Expr::MathFloor(v)
            | Expr::MathCeil(v)
            | Expr::MathTrunc(v)
            | Expr::MathAbs(v)
            | Expr::MathRound(v)
            | Expr::MathSqrt(v) => {
                let intrinsic = match expr {
                    Expr::MathFloor(_) => Intrinsic::MathFloor,
                    Expr::MathCeil(_) => Intrinsic::MathCeil,
                    Expr::MathTrunc(_) => Intrinsic::MathTrunc,
                    Expr::MathAbs(_) => Intrinsic::MathAbs,
                    Expr::MathRound(_) => Intrinsic::MathRound,
                    _ => Intrinsic::MathSqrt,
                };
                let value = self.number_operand(ctx, v)?;
                self.intrinsic(ctx, intrinsic, vec![value]).expect("number")
            }
            Expr::MathMin(values) | Expr::MathMax(values) => {
                let (intrinsic, empty) = if matches!(expr, Expr::MathMin(_)) {
                    (Intrinsic::MathMin, f64::INFINITY)
                } else {
                    (Intrinsic::MathMax, f64::NEG_INFINITY)
                };
                let mut result = self.number(ctx, empty);
                for value in values {
                    let value = self.number_operand(ctx, value)?;
                    result = self
                        .intrinsic(ctx, intrinsic, vec![result, value])
                        .expect("number");
                }
                result
            }
            Expr::Uint8ArrayNew(None) => {
                let zero = self.number(ctx, 0.0);
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![zero])
                    .expect("bytes")
            }
            Expr::Uint8ArrayNew(Some(arg)) | Expr::Uint8ArrayFrom(arg) => {
                let number = FP64Type::get(ctx).into();
                let numbers = ArrayType::get(ctx, number).into();
                let expected = matches!(&**arg, Expr::Array(_)).then_some(numbers);
                let value = self.expression_with(ctx, arg, expected)?;
                let value = self.defined(ctx, value)?;
                let value = self.arithmetic(ctx, value);
                match kind(ctx, value)? {
                    Kind::Number if matches!(expr, Expr::Uint8ArrayNew(_)) => self
                        .intrinsic(ctx, Intrinsic::BytesNew, vec![value])
                        .expect("bytes"),
                    Kind::Array => {
                        ensure!(
                            value.get_type(ctx) == numbers,
                            "Uint8Array sources must be number arrays"
                        );
                        self.intrinsic(ctx, Intrinsic::BytesFromArray, vec![value])
                            .expect("bytes")
                    }
                    Kind::ByteBuffer => self
                        .intrinsic(ctx, Intrinsic::BytesFromBuffer, vec![value])
                        .expect("bytes"),
                    Kind::Bytes => self
                        .intrinsic(ctx, Intrinsic::BytesCopy, vec![value])
                        .expect("bytes"),
                    _ => bail!("new Uint8Array takes a length, a number[] or a Uint8Array"),
                }
            }
            Expr::Uint8ArrayGet { array, index } => {
                let bytes = self.typed_operand(ctx, array, Kind::Bytes, "a Uint8Array")?;
                let index = self.number_operand(ctx, index)?;
                self.intrinsic(ctx, Intrinsic::BytesGet, vec![bytes, index])
                    .expect("byte")
            }
            Expr::Uint8ArraySet {
                array,
                index,
                value,
            } => {
                let bytes = self.typed_operand(ctx, array, Kind::Bytes, "a Uint8Array")?;
                let index = self.number_operand(ctx, index)?;
                let value = self.number_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::BytesSet, vec![bytes, index, value]);
                value
            }
            Expr::Uint8ArrayLength(array) => {
                let bytes = self.typed_operand(ctx, array, Kind::Bytes, "a Uint8Array")?;
                self.intrinsic(ctx, Intrinsic::BytesLength, vec![bytes])
                    .expect("length")
            }
            Expr::TextEncoderEncode(text) => {
                let text = self.string_operand(ctx, text)?;
                self.intrinsic(ctx, Intrinsic::TextEncode, vec![text])
                    .expect("bytes")
            }
            Expr::TextDecoderNew {
                label,
                fatal,
                ignore_bom,
            } => {
                ensure!(
                    matches!(&**label, Expr::Undefined)
                        || matches!(&**label, Expr::String(l) if l.eq_ignore_ascii_case("utf-8") || l.eq_ignore_ascii_case("utf8")),
                    "TextDecoder supports UTF-8 only"
                );
                let fatal = match &**fatal {
                    Expr::Undefined => self.boolean(ctx, false),
                    fatal => self.condition(ctx, fatal)?,
                };
                let ignore_bom = match &**ignore_bom {
                    Expr::Undefined => self.boolean(ctx, false),
                    ignore_bom => self.condition(ctx, ignore_bom)?,
                };
                self.intrinsic(ctx, Intrinsic::DecoderNew, vec![fatal, ignore_bom])
                    .expect("decoder")
            }
            Expr::TextDecoderDecode { decoder, input } => {
                let decoder = self.typed_operand(ctx, decoder, Kind::Decoder, "a TextDecoder")?;
                self.decode(ctx, decoder, Some(input), None)?
            }
            Expr::TextDecoderEncoding(decoder) => {
                self.typed_operand(ctx, decoder, Kind::Decoder, "a TextDecoder")?;
                self.string(ctx, "utf-8")
            }
            Expr::TextDecoderFatal(decoder) | Expr::TextDecoderIgnoreBom(decoder) => {
                let decoder = self.typed_operand(ctx, decoder, Kind::Decoder, "a TextDecoder")?;
                let intrinsic = if matches!(expr, Expr::TextDecoderFatal(_)) {
                    Intrinsic::DecoderFatal
                } else {
                    Intrinsic::DecoderIgnoreBom
                };
                self.intrinsic(ctx, intrinsic, vec![decoder]).expect("flag")
            }
            Expr::StringFromCharCode(_) | Expr::StringFromCharCodeSpread(_) => {
                bail!("fromCharCode uses UTF-16 code units; use fromCodePoint")
            }
            Expr::StringFromCodePoint(value) => {
                let value = self.number_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::StringFromCodePoint, vec![value])
                    .expect("string")
            }
            Expr::StringCodePointAt { string, index } => {
                let value = self.string_operand(ctx, string)?;
                let index = self.number_operand(ctx, index)?;
                self.intrinsic(ctx, Intrinsic::StringCodePointAt, vec![value, index])
                    .expect("code point")
            }
            Expr::JsonParse(text) => {
                let text = self.string_operand(ctx, text)?;
                self.intrinsic(ctx, Intrinsic::JsonParse, vec![text])
                    .expect("json")
            }
            Expr::JsonStringify(value) => self.stringify(ctx, value)?,
            Expr::JsonStringifyFull(value, replacer, space) => {
                ensure!(
                    matches!(&**replacer, Expr::Null | Expr::Undefined)
                        && matches!(&**space, Expr::Null | Expr::Undefined),
                    "{JSON_CALLBACKS}"
                );
                self.stringify(ctx, value)?
            }
            Expr::JsonParseWithReviver(text, reviver)
            | Expr::JsonParseReviver { text, reviver } => {
                ensure!(
                    matches!(&**reviver, Expr::Null | Expr::Undefined),
                    "{JSON_CALLBACKS}"
                );
                let text = self.string_operand(ctx, text)?;
                self.intrinsic(ctx, Intrinsic::JsonParse, vec![text])
                    .expect("json")
            }
            Expr::JsonStringifyPretty { .. } => bail!("{JSON_CALLBACKS}"),
            Expr::MathRandom => self
                .intrinsic(ctx, Intrinsic::MathRandom, vec![])
                .expect("number"),
            Expr::CryptoRandomUUID => self
                .intrinsic(ctx, Intrinsic::RandomUuid, vec![])
                .expect("uuid"),
            Expr::TypeOf(value) => {
                let value = self.expression(ctx, value)?;
                self.type_of(ctx, value)?
            }
            Expr::EnvGet(name) => {
                let name = self.string(ctx, name);
                self.intrinsic(ctx, Intrinsic::EnvGet, vec![name])
                    .expect("value")
            }
            Expr::ObjectKeys(object) if matches!(**object, Expr::ProcessEnv) => self
                .intrinsic(ctx, Intrinsic::EnvKeys, vec![])
                .expect("keys"),
            Expr::ObjectKeys(object) => {
                let object = self.expression(ctx, object)?;
                ensure!(
                    kind(ctx, object)? == Kind::Json,
                    "Object.keys requires a JSON snapshot"
                );
                self.intrinsic(ctx, Intrinsic::JsonKeys, vec![object])
                    .expect("keys")
            }
            Expr::PerformanceNow => self
                .intrinsic(ctx, Intrinsic::PerformanceNow, vec![])
                .expect("time"),
            Expr::DateNow => self
                .intrinsic(ctx, Intrinsic::DateNow, vec![])
                .expect("time"),
            Expr::ErrorNew(message) => {
                let message = match message {
                    Some(message) => self.string_operand(ctx, message)?,
                    None => self.string(ctx, ""),
                };
                self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, ErrorKind::Any, message))
            }
            Expr::TypeErrorNew(message) | Expr::RangeErrorNew(message) => {
                let kind = if matches!(expr, Expr::TypeErrorNew(_)) {
                    ErrorKind::TypeError
                } else {
                    ErrorKind::RangeError
                };
                let message = self.string_operand(ctx, message)?;
                self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, kind, message))
            }
            // `new Error(message, { cause })`: causes are strings (accepted deviation).
            Expr::ErrorNewWithCause { message, cause } => {
                let message = self.string_operand(ctx, message)?;
                let error =
                    self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, ErrorKind::Any, message));
                self.error_cause(ctx, error, cause)?;
                error
            }
            Expr::ErrorNewWithOptions {
                kind: error_kind,
                message,
                options,
            } => {
                let kind = match error_kind {
                    0 => ErrorKind::Any,
                    1 => ErrorKind::TypeError,
                    2 => ErrorKind::RangeError,
                    _ => bail!("only Error, TypeError and RangeError are supported yet"),
                };
                let cause = match &**options {
                    Expr::New {
                        class_name, args, ..
                    } if self
                        .shapes
                        .literal_fields(class_name)
                        .is_some_and(|names| names == ["cause"]) =>
                    {
                        &args[0]
                    }
                    Expr::Object(fields) if fields.len() == 1 && fields[0].0 == "cause" => {
                        &fields[0].1
                    }
                    _ => bail!("error options must be a literal {{ cause }}"),
                };
                let message = self.string_operand(ctx, message)?;
                let error = self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, kind, message));
                self.error_cause(ctx, error, cause)?;
                error
            }
            Expr::ErrorMessage(error) => {
                let error = self.expression(ctx, error)?;
                ensure!(
                    kind(ctx, error)? == Kind::Exception,
                    ".message is only supported on caught exceptions and Error objects"
                );
                self.emit_value(ctx, |ctx| ExceptionMessageOp::new(ctx, error))
            }
            Expr::InstanceOf {
                expr: value, ty, ..
            } => {
                let value = self.expression(ctx, value)?;
                let error = match ty.as_str() {
                    "Error" => ErrorKind::Any,
                    "TypeError" => ErrorKind::TypeError,
                    "RangeError" => ErrorKind::RangeError,
                    other => bail!("instanceof {other} is not supported yet"),
                };
                match kind(ctx, value)? {
                    Kind::Exception => {
                        self.emit_value(ctx, |ctx| ExceptionIsOp::new(ctx, error, value))
                    }
                    _ => self.boolean(ctx, false),
                }
            }
            Expr::Await(promise) => {
                ensure!(self.is_async, "await is only valid in async functions");
                let promise = self.expression(ctx, promise)?;
                ensure!(
                    TypedHandle::<PromiseType>::from_handle(promise.get_type(ctx), ctx).is_ok(),
                    "await of a non-promise value is not supported yet"
                );
                let op = AwaitOp::new(ctx, promise).map_err(|e| anyhow!("{}", e.disp(ctx)))?;
                self.append_value(ctx, &op)
            }
            Expr::PropertyGet {
                object, property, ..
            } => {
                let value = self.expression(ctx, object)?;
                // Property access on `undefined` throws TypeError, in ECMAScript and here.
                let value = self.defined(ctx, value)?;
                match (kind(ctx, value)?, property.as_str()) {
                    (Kind::Url, field) => self.url_field(ctx, value, field)?,
                    (Kind::Decoder, "encoding") => self.string(ctx, "utf-8"),
                    (Kind::Decoder, "fatal" | "ignoreBOM") => {
                        let intrinsic = if property == "fatal" {
                            Intrinsic::DecoderFatal
                        } else {
                            Intrinsic::DecoderIgnoreBom
                        };
                        self.intrinsic(ctx, intrinsic, vec![value]).expect("flag")
                    }
                    (Kind::String, "length") => {
                        let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, value));
                        self.emit_value(ctx, |ctx| {
                            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                        })
                    }
                    (Kind::Array, "length") => {
                        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, value));
                        self.emit_value(ctx, |ctx| {
                            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                        })
                    }
                    (Kind::Bytes, "buffer") => self
                        .intrinsic(ctx, Intrinsic::BytesBuffer, vec![value])
                        .expect("buffer"),
                    (Kind::Bytes, "byteOffset") => self
                        .intrinsic(ctx, Intrinsic::BytesOffset, vec![value])
                        .expect("offset"),
                    (Kind::ByteBuffer, "byteLength") => self
                        .intrinsic(ctx, Intrinsic::BufferLength, vec![value])
                        .expect("length"),
                    (Kind::Bytes, "length" | "byteLength") => self
                        .intrinsic(ctx, Intrinsic::BytesLength, vec![value])
                        .expect("length"),
                    (Kind::Json, "length") => self
                        .intrinsic(ctx, Intrinsic::JsonLength, vec![value])
                        .expect("length"),
                    (Kind::Json, name) => {
                        let name = self.string(ctx, name);
                        self.intrinsic(ctx, Intrinsic::JsonMember, vec![value, name])
                            .expect("member")
                    }
                    (Kind::Settled, field) => {
                        let field = match field {
                            "status" => SettledField::Status,
                            "value" => SettledField::Value,
                            "reason" => SettledField::Reason,
                            other => bail!("settled results have no property {other}"),
                        };
                        let op = SettledFieldOp::new(ctx, field, value).expect("settled");
                        self.append_value(ctx, &op)
                    }
                    (
                        Kind::Headers
                        | Kind::Response
                        | Kind::Request
                        | Kind::Writable
                        | Kind::Writer
                        | Kind::Readable
                        | Kind::Reader
                        | Kind::ByobReader
                        | Kind::ReadResult,
                        name,
                    ) => self.web_member(ctx, value, name, None)?,
                    (Kind::Abort, "signal") => value,
                    (Kind::Abort, "aborted") => self
                        .intrinsic(ctx, Intrinsic::AbortAborted, vec![value])
                        .expect("aborted"),
                    (Kind::Stats, "size") => self
                        .intrinsic(ctx, Intrinsic::StatsSize, vec![value])
                        .expect("size"),
                    (Kind::Exception, "cause") => self
                        .intrinsic(ctx, Intrinsic::ExceptionCause, vec![value])
                        .expect("cause"),
                    (Kind::Exception, "name") => self
                        .intrinsic(ctx, Intrinsic::ExceptionName, vec![value])
                        .expect("name"),
                    (Kind::Exception, "message") => {
                        self.emit_value(ctx, |ctx| ExceptionMessageOp::new(ctx, value))
                    }
                    (Kind::Variant, field) => self.variant_member(ctx, object, value, field)?,
                    (Kind::Record, field) => {
                        let field = field.to_string();
                        let op = FieldGetOp::new(ctx, value, field.clone()).map_err(|_| {
                            anyhow!(
                                "property {field} does not exist on this shape (subset rule R1)"
                            )
                        })?;
                        self.append_value(ctx, &op)
                    }
                    (_, property) => bail!("property {property} is not supported on this type yet"),
                }
            }
            Expr::PropertySet {
                object,
                property,
                value,
            } => self.set_field(ctx, object, property, value)?,
            Expr::PutValueSet {
                target, key, value, ..
            } => match key.as_ref() {
                Expr::String(field) => self.set_field(ctx, target, field, value)?,
                index => self.set_index(ctx, target, index, value)?,
            },
            Expr::New {
                class_name, args, ..
            } if class_name == "AbortController" && !self.shapes.is_user_type(class_name) => {
                ensure!(args.is_empty(), "new AbortController takes no arguments");
                self.intrinsic(ctx, Intrinsic::AbortNew, vec![])
                    .expect("controller")
            }
            Expr::New {
                class_name, args, ..
            } if matches!(class_name.as_str(), "Headers" | "Response" | "Request")
                && !self.shapes.is_user_type(class_name) =>
            {
                self.web_new(ctx, class_name, args)?
            }
            Expr::FetchWithOptions {
                url,
                method,
                body,
                headers,
                headers_dynamic,
                signal,
                redirect,
            } => {
                ensure!(
                    signal.is_none() && redirect.is_none(),
                    "fetch signal and redirect options are not supported yet"
                );
                let url = self.url_string(ctx, url)?;
                let method = self.typed_operand(ctx, method, Kind::String, "a method string")?;
                let list = match headers_dynamic {
                    Some(dynamic) => self.headers_init(ctx, Some(dynamic))?,
                    None => {
                        let list = self
                            .intrinsic(ctx, Intrinsic::HeadersNew, vec![])
                            .expect("headers");
                        for (name, value) in headers {
                            let name = self.string(ctx, name);
                            let value =
                                self.typed_operand(ctx, value, Kind::String, "a header value")?;
                            self.intrinsic(ctx, Intrinsic::HeadersAppend, vec![list, name, value]);
                        }
                        list
                    }
                };
                let (body, text) = self.body_init(ctx, Some(body))?;
                let text = self.boolean(ctx, text);
                self.intrinsic(ctx, Intrinsic::Fetch, vec![url, method, list, body, text])
                    .expect("promise")
            }
            Expr::NativeMethodCall {
                module,
                class_name: Some(class),
                object: None,
                method,
                args,
            } if matches!(module.as_str(), "stream" | "node:stream")
                && class == "Writable"
                && method == "toWeb" =>
            {
                let intrinsic = match args.as_slice() {
                    [Expr::ProcessStdout] => Intrinsic::WritableStdout,
                    [Expr::ProcessStderr] => Intrinsic::WritableStderr,
                    _ => bail!("Writable.toWeb supports process.stdout and process.stderr"),
                };
                self.intrinsic(ctx, intrinsic, vec![]).expect("stream")
            }
            Expr::NativeMethodCall {
                object: Some(object),
                method,
                args,
                ..
            } => {
                let receiver = self.expression(ctx, object)?;
                let receiver = self.defined(ctx, receiver)?;
                let call = !args.is_empty()
                    || !matches!(
                        method.as_str(),
                        "status"
                            | "statusText"
                            | "ok"
                            | "headers"
                            | "bodyUsed"
                            | "url"
                            | "method"
                            | "locked"
                            | "body"
                    );
                ensure!(
                    matches!(
                        kind(ctx, receiver)?,
                        Kind::Headers
                            | Kind::Response
                            | Kind::Request
                            | Kind::Writable
                            | Kind::Writer
                            | Kind::Readable
                            | Kind::Reader
                            | Kind::ByobReader
                            | Kind::ReadResult
                    ),
                    "{method} is not supported on this type yet"
                );
                self.web_member(ctx, receiver, method, call.then_some(args.as_slice()))?
            }
            Expr::New {
                class_name, args, ..
            } => {
                let names = self
                    .shapes
                    .literal_fields(class_name)
                    .with_context(|| format!("class {class_name} is not supported yet"))?;
                ensure!(
                    names.len() == args.len(),
                    "object literal arity differs from its shape"
                );
                self.record_literal(ctx, &names, args, expected)?
            }
            // `[a, b]` where a tuple is expected: its record `{ _0: a, _1: b }`.
            Expr::Array(items)
                if expected.is_some_and(|ty| {
                    TypedHandle::<RecordType>::from_handle(
                        optional_value(ctx, ty).unwrap_or(ty),
                        ctx,
                    )
                    .is_ok_and(|record| record.deref(ctx).is_tuple())
                }) =>
            {
                let names: Vec<String> = (0..items.len()).map(|i| format!("_{i}")).collect();
                self.record_literal(ctx, &names, items, expected)?
            }
            Expr::Array(items) => {
                let element = match expected.map(|ty| {
                    TypedHandle::<ArrayType>::from_handle(
                        optional_value(ctx, ty).unwrap_or(ty),
                        ctx,
                    )
                }) {
                    Some(Ok(array)) => Some(array.deref(ctx).element()),
                    _ => None,
                };
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    let value = self.expression_with(ctx, item, element)?;
                    let value = match element {
                        Some(element) => self.fit(ctx, value, element)?,
                        None => value,
                    };
                    values.push(value);
                }
                let element = match (element, values.first()) {
                    (Some(element), _) => element,
                    (None, Some(first)) => {
                        let first = first.get_type(ctx);
                        let inner = optional_value(ctx, first).unwrap_or(first);
                        let mut states = absence(ctx, first);
                        for value in &values[1..] {
                            let ty = value.get_type(ctx);
                            ensure!(
                                optional_value(ctx, ty).unwrap_or(ty) == inner,
                                "array literals must have one element type"
                            );
                            states = match (states, absence(ctx, ty)) {
                                (None, next) => next,
                                (current, None) => current,
                                (Some(a), Some(b)) => Some(if a == b {
                                    a
                                } else {
                                    perry_mir::attrs::Absence::Both
                                }),
                            };
                        }
                        states.map_or(inner, |states| {
                            perry_mir::types::OptionalType::get_with(ctx, inner, states).into()
                        })
                    }
                    (None, None) => bail!("an empty array literal needs a declared element type"),
                };
                let values = values
                    .into_iter()
                    .map(|v| self.fit(ctx, v, element))
                    .collect::<Result<_>>()?;
                self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, values))
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => {
                let slot = self.local(*array_id)?;
                let array = self.load(ctx, slot)?;
                let element = self.array_element(ctx, array)?;
                let value = self.expression_with(ctx, value, Some(element))?;
                let value = self.fit(ctx, value, element)?;
                self.emit(ctx, |ctx| ArrayPushOp::new(ctx, array, value));
                let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, array));
                self.emit_value(ctx, |ctx| {
                    ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                })
            }
            Expr::IndexGet { object, index } => {
                let array = self.expression(ctx, object)?;
                let array = self.defined(ctx, array)?;
                match kind(ctx, array)? {
                    Kind::String => {
                        let index = self.number_operand(ctx, index)?;
                        return Ok(self
                            .intrinsic(ctx, Intrinsic::StringIndex, vec![array, index])
                            .expect("character"));
                    }
                    Kind::Bytes => {
                        let index = self.number_operand(ctx, index)?;
                        return Ok(self
                            .intrinsic(ctx, Intrinsic::BytesGet, vec![array, index])
                            .expect("byte"));
                    }
                    Kind::Json => {
                        let index = self.expression(ctx, index)?;
                        let index = self.defined(ctx, index)?;
                        let intrinsic = match kind(ctx, index)? {
                            Kind::String => Intrinsic::JsonMember,
                            Kind::Number => Intrinsic::JsonIndex,
                            _ => bail!("JSON lookup requires a string or number key"),
                        };
                        return Ok(self
                            .intrinsic(ctx, intrinsic, vec![array, index])
                            .expect("json"));
                    }
                    Kind::Record => {
                        let field = match &**index {
                            Expr::String(name) => name.clone(),
                            Expr::Number(n) if n.fract() == 0.0 && *n >= 0.0 => {
                                format!("_{}", *n as u64)
                            }
                            Expr::Integer(n) if *n >= 0 => format!("_{n}"),
                            _ => bail!(
                                "closed records require literal keys; tuples require literal positions"
                            ),
                        };
                        let op = FieldGetOp::new(ctx, array, field.clone())
                            .map_err(|_| anyhow!("record has no field {field}"))?;
                        return Ok(self.append_value(ctx, &op));
                    }
                    _ => {}
                }
                ensure!(
                    kind(ctx, array)? == Kind::Array,
                    "indexing is only supported on arrays so far"
                );
                let index = self.index(ctx, index)?;
                let op = ArrayGetOp::new(ctx, array, index)?;
                self.append_value(ctx, &op)
            }
            Expr::IndexSet {
                object,
                index,
                value,
            } => self.set_index(ctx, object, index, value)?,
            other => bail!(
                "MIR backend does not support expression {} yet",
                expression_name(other)
            ),
        };
        self.narrow_primitive(ctx, expr, value)
    }

    /// Whether `expr` is array destructuring's guard for the iterator protocol, which is
    /// false for MIR arrays: they are plain arrays, and `Array.prototype[Symbol.iterator]`
    /// cannot be patched. Other sources are an error.
    fn iterator_guard(&mut self, ctx: &mut Context, expr: &Expr) -> Result<bool> {
        let source = match expr {
            Expr::ArrayIterationPatched => return Ok(true),
            Expr::NativeMethodCall {
                module,
                object: None,
                method,
                args,
                ..
            } if module == "__perry_runtime" && method == "arrayDestructureNeedsIterator" => {
                let [source] = args.as_slice() else {
                    bail!("arrayDestructureNeedsIterator takes one value");
                };
                source
            }
            _ => return Ok(false),
        };
        let source = self.expression(ctx, source)?;
        ensure!(
            kind(ctx, source)? == Kind::Array,
            "array destructuring is supported for arrays so far"
        );
        Ok(true)
    }

    /// The array literal given to a promise combinator: plain values among promises of `T`
    /// become promises fulfilled with them, as `Promise.all` treats them.
    fn combinator_inputs(&mut self, ctx: &mut Context, items: &[Expr]) -> Result<Value> {
        // `Promise.reject(e)` takes its settled type from the other inputs: lower it last.
        let mut lowered: Vec<Option<Value>> = Vec::with_capacity(items.len());
        for item in items {
            lowered.push(
                if promise_call(item).is_some_and(|(name, _)| name == "reject") {
                    None
                } else {
                    let value = self.expression(ctx, item)?;
                    Some(self.defined(ctx, value)?)
                },
            );
        }
        let known: Vec<Value> = lowered.iter().flatten().copied().collect();
        let promised = known.iter().find_map(|value| {
            TypedHandle::<PromiseType>::from_handle(value.get_type(ctx), ctx)
                .ok()
                .map(|promise| promise.deref(ctx).value())
        });
        let settled = match promised {
            Some(settled) => settled,
            None => match known.first() {
                Some(first) => first.get_type(ctx),
                None => self.void_type(ctx),
            },
        };
        let promise: TypeHandle = PromiseType::get(ctx, settled).into();
        let mut values = Vec::with_capacity(items.len());
        for (item, value) in items.iter().zip(lowered) {
            values.push(match value {
                Some(value) => value,
                None => {
                    let (name, reason) = promise_call(item).expect("checked");
                    self.promise_settled(ctx, name, reason, Some(settled))?
                }
            });
        }
        let mut inputs = Vec::with_capacity(values.len());
        for value in values {
            let value = if value.get_type(ctx) == promise {
                value
            } else {
                let value = self.fit(ctx, value, settled)?;
                self.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, value))
            };
            inputs.push(value);
        }
        Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, promise, inputs)))
    }

    /// `Promise.resolve(value)` (a promise passes through) or `Promise.reject(reason)`, a
    /// promise of `settled` (`undefined` when nothing says).
    fn promise_settled(
        &mut self,
        ctx: &mut Context,
        name: &str,
        value: &Expr,
        settled: Option<TypeHandle>,
    ) -> Result<Value> {
        let value = self.expression(ctx, value)?;
        let value = self.defined(ctx, value)?;
        if name == "resolve" {
            if TypedHandle::<PromiseType>::from_handle(value.get_type(ctx), ctx).is_ok() {
                return Ok(value);
            }
            return Ok(self.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, value)));
        }
        let settled = match settled {
            Some(settled) => settled,
            None => self.void_type(ctx),
        };
        Ok(self.emit_value(ctx, |ctx| PromiseRejectedOp::new(ctx, value, settled)))
    }

    /// The settled type of `Promise<void>`: `undefined` as `number | undefined`.
    fn void_type(&self, ctx: &mut Context) -> TypeHandle {
        let number = FP64Type::get(ctx).into();
        perry_mir::types::OptionalType::get(ctx, number).into()
    }

    /// Set the `cause` of a new error: a string (accepted profile).
    fn error_cause(&mut self, ctx: &mut Context, error: Value, cause: &Expr) -> Result<()> {
        let cause = self.typed_operand(ctx, cause, Kind::String, "a string error cause")?;
        self.intrinsic(ctx, Intrinsic::ExceptionSetCause, vec![error, cause]);
        Ok(())
    }

    /// An object literal with spreads, which Perry HIR lowers to a source-ordered IIFE over
    /// `{}` (`__o`): `js_object_assign_one(__o, source)` per spread and `__o[key] = value`
    /// per static member. Over JSON values it builds a JSON object; typed records do not
    /// spread yet.
    fn object_spread(
        &mut self,
        ctx: &mut Context,
        target: LocalId,
        body: &[Stmt],
    ) -> Result<Value> {
        const UNSUPPORTED: &str = "object spread is supported over JSON values (JSON.parse, \
            response.json()) with static keys; computed keys, methods and accessors are not";
        let object = self
            .intrinsic(ctx, Intrinsic::JsonObject, vec![])
            .expect("object");
        let is_target = |expr: &Expr| matches!(expr, Expr::LocalGet(id) if *id == target);
        for statement in body {
            match statement {
                Stmt::Expr(Expr::Call { callee, args, .. })
                    if matches!(&**callee, Expr::ExternFuncRef { name, .. }
                        if name == "js_object_assign_one")
                        && args.len() == 2
                        && is_target(&args[0]) =>
                {
                    let source = self.expression(ctx, &args[1])?;
                    // An `Error` has no own enumerable properties: it adds nothing.
                    if matches!(kind(ctx, source), Ok(Kind::Exception)) {
                        continue;
                    }
                    let Some(source) = self.json_operand(ctx, source)? else {
                        bail!("{UNSUPPORTED}");
                    };
                    self.intrinsic(ctx, Intrinsic::JsonAssign, vec![object, source]);
                }
                Stmt::Expr(Expr::IndexSet {
                    object: set,
                    index,
                    value,
                }) if is_target(set) => {
                    let Expr::String(key) = &**index else {
                        bail!("{UNSUPPORTED}");
                    };
                    let key = self.string(ctx, key);
                    let value = self.json_value(ctx, value)?;
                    self.intrinsic(ctx, Intrinsic::JsonSet, vec![object, key, value]);
                }
                Stmt::Return(Some(result)) if is_target(result) => {}
                _ => bail!("{UNSUPPORTED}"),
            }
        }
        Ok(object)
    }

    /// A JSON value, or `Json | undefined` (an array element) as one: the JSON `undefined` is
    /// the null address. `None` for other types.
    fn json_operand(&mut self, ctx: &mut Context, value: Value) -> Result<Option<Value>> {
        let ty = value.get_type(ctx);
        if ty.deref(ctx).is::<JsonType>() {
            return Ok(Some(value));
        }
        if !optional_value(ctx, ty).is_some_and(|inner| inner.deref(ctx).is::<JsonType>()) {
            return Ok(None);
        }
        let json = JsonType::get(ctx).into();
        let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
        self.choose(
            ctx,
            json,
            undefined,
            |b, ctx| {
                Ok(b.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])
                    .expect("json"))
            },
            |b, ctx| b.defined(ctx, value),
        )
        .map(Some)
    }

    /// A member value of a JSON object literal: a JSON value, or a number, string, boolean
    /// or `null` converted to one.
    fn json_value(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        if matches!(expr, Expr::Null) {
            return Ok(self
                .intrinsic(ctx, Intrinsic::JsonNull, vec![])
                .expect("json"));
        }
        let value = self.expression(ctx, expr)?;
        if let Some(json) = self.json_operand(ctx, value)? {
            return Ok(json);
        }
        let intrinsic = match kind(ctx, value)? {
            Kind::Number => Intrinsic::JsonFromNumber,
            Kind::String => Intrinsic::JsonFromString,
            Kind::Boolean => Intrinsic::JsonFromBool,
            _ => bail!(
                "members of a spread object literal are JSON values, numbers, strings, \
                 booleans or null so far"
            ),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value]).expect("json"))
    }

    /// `typeof value`, including the active case of a closed primitive union.
    fn type_of(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if optional_value(ctx, value.get_type(ctx)).is_some() {
            let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            return self.choose_string(
                ctx,
                undefined,
                |b, ctx| Ok(b.string(ctx, "undefined")),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                    b.choose_string(
                        ctx,
                        null,
                        |b, ctx| Ok(b.string(ctx, "object")),
                        |b, ctx| {
                            let defined = b.defined(ctx, value)?;
                            b.type_of(ctx, defined)
                        },
                    )
                },
            );
        }
        let name = match kind(ctx, value)? {
            Kind::NumberString => {
                return Ok(self.emit_value(ctx, |ctx| {
                    perry_mir::variants::VariantTagOp::new(ctx, value)
                }));
            }
            Kind::BigInt => "bigint",
            Kind::Number => "number",
            Kind::String => "string",
            Kind::Boolean => "boolean",
            Kind::Json => {
                return Ok(self
                    .intrinsic(ctx, Intrinsic::JsonTypeOf, vec![value])
                    .expect("type"));
            }
            _ => "object",
        };
        Ok(self.string(ctx, name))
    }

    /// `JSON.stringify(value)`.
    fn stringify(&mut self, ctx: &mut Context, value: &Expr) -> Result<Value> {
        let value = self.expression(ctx, value)?;
        Ok(self.emit_value(ctx, |ctx| JsonStringifyOp::new(ctx, value)))
    }

    fn string(&mut self, ctx: &mut Context, text: &str) -> Value {
        let op = StringConstOp::new(ctx, text);
        self.append_value(ctx, &op)
    }

    /// An expression that must be a string (`undefined` throws `TypeError`).
    fn string_operand(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.defined(ctx, value)?;
        ensure!(
            kind(ctx, value)? == Kind::String,
            "a string is required here"
        );
        Ok(value)
    }

    /// A slot inserted at the start of the entry block, dominating every use.
    fn entry_slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        let entry = self.region.deref(ctx).get_head().expect("entry block");
        let op = LocalOp::new(ctx, ty);
        let mut at_entry = IRInserter::<DummyListener>::new_at_block_start(entry);
        at_entry.append_op(ctx, &op);
        op.get_result(ctx)
    }

    fn slot_type(&self, ctx: &Context, slot: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<perry_mir::types::SlotType>::from_handle(slot.get_type(ctx), ctx)
                .map_err(|_| anyhow!("not a slot"))?
                .deref(ctx)
                .value(),
        )
    }

    fn array_element(&self, ctx: &Context, array: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
                .map_err(|_| anyhow!("expected an array"))?
                .deref(ctx)
                .element(),
        )
    }

    /// An ECMAScript number used as an array index (R8: reads miss with `undefined`).
    fn index(&mut self, ctx: &mut Context, index: &Expr) -> Result<Value> {
        let index = self.expression(ctx, index)?;
        let index = self.arithmetic(ctx, index);
        ensure!(
            kind(ctx, index)? == Kind::Number,
            "array indices must be numbers"
        );
        Ok(index)
    }

    fn is_bool_type(&self, ctx: &Context, ty: TypeHandle) -> bool {
        ty.deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|int| int.width() == 1)
    }

    /// An expression used as a number (`undefined` becomes `NaN`).
    fn number_operand(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.arithmetic(ctx, value);
        ensure!(
            kind(ctx, value)? == Kind::Number,
            "a number is required here"
        );
        Ok(value)
    }

    /// An optional numeric argument, or `default`.
    fn number_or(&mut self, ctx: &mut Context, expr: Option<&Expr>, default: f64) -> Result<Value> {
        match expr {
            Some(Expr::Undefined) | None => Ok(self.number(ctx, default)),
            Some(expr) => self.number_operand(ctx, expr),
        }
    }

    fn typed_operand(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
        want: Kind,
        what: &str,
    ) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.defined(ctx, value)?;
        ensure!(kind(ctx, value)? == want, "{what} is required here");
        Ok(value)
    }

    fn regex_test(&mut self, ctx: &mut Context, regex: Value, text: Value) -> Value {
        let index = self
            .intrinsic(ctx, Intrinsic::RegexSearch, vec![text, regex])
            .expect("index");
        let zero = self.number(ctx, 0.0);
        self.emit_value(ctx, |ctx| {
            MirCompare::new(ctx, CompareKind::Ge, index, zero)
        })
    }

    /// `object.method(args)` on built-in values.
    fn method(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<Value>> {
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        let arg = |index: usize| args.get(index);
        let method = if method == decoder::DECODE {
            "decode"
        } else {
            method
        };
        let value = match (kind(ctx, receiver)?, method) {
            (Kind::String, "split") => {
                let [separator] = args else {
                    bail!("string.split requires one string separator");
                };
                let separator = self.string_operand(ctx, separator)?;
                self.intrinsic(ctx, Intrinsic::StringSplit, vec![receiver, separator])
            }
            (Kind::Regex, "test") => {
                let [text] = args else {
                    bail!("literal regex.test requires one string");
                };
                let text = self.string_operand(ctx, text)?;
                Some(self.regex_test(ctx, receiver, text))
            }
            (Kind::String, "search") => {
                let [regex] = args else {
                    bail!("string.search requires one literal regex");
                };
                let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                self.intrinsic(ctx, Intrinsic::RegexSearch, vec![receiver, regex])
            }
            (Kind::Decoder, "decode") => {
                ensure!(
                    args.len() <= 2,
                    "TextDecoder.decode takes bytes and stream options"
                );
                Some(self.decode(ctx, receiver, arg(0), arg(1))?)
            }
            (Kind::Url, "toString" | "toJSON") => Some(self.url_field(ctx, receiver, "href")?),
            (Kind::SearchParams, "append") => {
                let [name, value] = args else {
                    bail!("searchParams.append needs a name and value");
                };
                let name = self.string_operand(ctx, name)?;
                let value = self.string_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::UrlAppend, vec![receiver, name, value]);
                let ty = self.void_type(ctx);
                Some(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
            }
            (
                Kind::Headers
                | Kind::Response
                | Kind::Request
                | Kind::Writable
                | Kind::Writer
                | Kind::Readable
                | Kind::Reader
                | Kind::ByobReader,
                _,
            ) => {
                return self.web_member(ctx, receiver, method, Some(args)).map(Some);
            }
            (Kind::Abort, "abort") => {
                ensure!(args.is_empty(), "abort reasons are not supported yet");
                self.intrinsic(ctx, Intrinsic::AbortAbort, vec![receiver]);
                return Ok(None);
            }
            (Kind::Stats, "isFile" | "isDirectory") => {
                let intrinsic = if method == "isFile" {
                    Intrinsic::StatsIsFile
                } else {
                    Intrinsic::StatsIsDirectory
                };
                self.intrinsic(ctx, intrinsic, vec![receiver])
            }
            (Kind::String, "charCodeAt") => {
                bail!("charCodeAt uses UTF-16 code units; use codePointAt with scalar indices")
            }
            (Kind::String, "charAt" | "codePointAt" | "at") => {
                let position = self.number_or(ctx, arg(0), 0.0)?;
                let intrinsic = if method == "codePointAt" {
                    Intrinsic::StringCodePointAt
                } else {
                    ensure!(method != "at", "String.prototype.at is not supported yet");
                    Intrinsic::StringCharAt
                };
                self.intrinsic(ctx, intrinsic, vec![receiver, position])
            }
            (Kind::String, "slice" | "substring") => {
                let start = self.number_or(ctx, arg(0), 0.0)?;
                let end = self.number_or(ctx, arg(1), f64::INFINITY)?;
                let intrinsic = if method == "slice" {
                    Intrinsic::StringSlice
                } else {
                    Intrinsic::StringSubstring
                };
                self.intrinsic(ctx, intrinsic, vec![receiver, start, end])
            }
            (Kind::String, "indexOf" | "lastIndexOf" | "includes" | "startsWith" | "endsWith") => {
                let needle = arg(0).context("a search string is required")?;
                let needle = self.typed_operand(ctx, needle, Kind::String, "a search string")?;
                let default = if matches!(method, "lastIndexOf" | "endsWith") {
                    f64::INFINITY
                } else {
                    0.0
                };
                let position = self.number_or(ctx, arg(1), default)?;
                let args = vec![receiver, needle, position];
                match method {
                    "indexOf" => self.intrinsic(ctx, Intrinsic::StringIndexOf, args),
                    "lastIndexOf" => self.intrinsic(ctx, Intrinsic::StringLastIndexOf, args),
                    "startsWith" => self.intrinsic(ctx, Intrinsic::StringStartsWith, args),
                    "endsWith" => self.intrinsic(ctx, Intrinsic::StringEndsWith, args),
                    _ => {
                        let found = self
                            .intrinsic(ctx, Intrinsic::StringIndexOf, args)
                            .expect("index");
                        let missing = self.number(ctx, -1.0);
                        Some(self.compare(ctx, CompareKind::Ne, found, missing))
                    }
                }
            }
            (
                Kind::String,
                "toLowerCase" | "toUpperCase" | "trim" | "toLocaleLowerCase" | "toLocaleUpperCase",
            ) => {
                let intrinsic = match method {
                    "trim" => Intrinsic::StringTrim,
                    "toLowerCase" | "toLocaleLowerCase" => Intrinsic::StringLower,
                    _ => Intrinsic::StringUpper,
                };
                self.intrinsic(ctx, intrinsic, vec![receiver])
            }
            (Kind::String, "repeat") => {
                let count = self.number_or(ctx, arg(0), 0.0)?;
                self.intrinsic(ctx, Intrinsic::StringRepeat, vec![receiver, count])
            }
            (Kind::String, "toString") => Some(receiver),
            (Kind::Bytes, "subarray" | "slice") => {
                let start = self.number_or(ctx, arg(0), 0.0)?;
                let end = self.number_or(ctx, arg(1), f64::INFINITY)?;
                let intrinsic = if method == "subarray" {
                    Intrinsic::BytesSubarray
                } else {
                    Intrinsic::BytesSlice
                };
                self.intrinsic(ctx, intrinsic, vec![receiver, start, end])
            }
            (Kind::Bytes, "set") => {
                let source = arg(0).context("set needs a source")?;
                let source = self.expression(ctx, source)?;
                let source = self.defined(ctx, source)?;
                let source = match kind(ctx, source)? {
                    Kind::Bytes => source,
                    Kind::Array => self
                        .intrinsic(ctx, Intrinsic::BytesFromArray, vec![source])
                        .expect("bytes"),
                    _ => bail!("Uint8Array.set needs a Uint8Array or number[] source"),
                };
                let offset = self.number_or(ctx, arg(1), 0.0)?;
                self.intrinsic(ctx, Intrinsic::BytesSetFrom, vec![receiver, source, offset]);
                None
            }
            (Kind::Bytes, "fill") => {
                let value = self.number_or(ctx, arg(0), 0.0)?;
                let start = self.number_or(ctx, arg(1), 0.0)?;
                let end = self.number_or(ctx, arg(2), f64::INFINITY)?;
                self.intrinsic(ctx, Intrinsic::BytesFill, vec![receiver, value, start, end]);
                Some(receiver)
            }
            (Kind::Bytes, "$$cryptoFillRandom") => {
                self.intrinsic(ctx, Intrinsic::BytesFillRandom, vec![receiver])
            }
            (Kind::Date, "getTime" | "valueOf") => {
                self.intrinsic(ctx, Intrinsic::DateTime, vec![receiver])
            }
            (Kind::Date, "toISOString" | "toJSON") => {
                let intrinsic = if method == "toJSON" {
                    Intrinsic::DateJson
                } else {
                    Intrinsic::DateIso
                };
                self.intrinsic(ctx, intrinsic, vec![receiver])
            }
            (_, method) => bail!("method {method} is not supported on this type yet"),
        };
        Ok(value)
    }

    fn is_optional(&self, ctx: &Context, value: Value) -> bool {
        optional_value(ctx, value.get_type(ctx)).is_some()
    }

    /// ECMAScript coercion of `T | undefined` for arithmetic, ordering and concatenation:
    /// `undefined` becomes `NaN` or `"undefined"`. Other values pass through.
    fn arithmetic(&mut self, ctx: &mut Context, value: Value) -> Value {
        if value.get_type(ctx).deref(ctx).is::<JsonType>() {
            return self
                .intrinsic(ctx, Intrinsic::JsonToNumber, vec![value])
                .expect("number");
        }
        let Some(inner) = optional_value(ctx, value.get_type(ctx)) else {
            return value;
        };
        if inner.deref(ctx).is::<pliron::builtin::types::FP64Type>() {
            self.emit_value(ctx, |ctx| ToNumberOp::new(ctx, value))
        } else if inner.deref(ctx).is::<StringType>() {
            self.emit_value(ctx, |ctx| ToStringOp::new(ctx, value))
        } else {
            value
        }
    }

    /// The defined value of `T | undefined`; throws `TypeError` on `undefined`.
    fn defined(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if !self.is_optional(ctx, value) {
            return Ok(value);
        }
        let op = UnwrapOp::new(ctx, value)?;
        Ok(self.append_value(ctx, &op))
    }

    /// View `value` as `T | undefined`.
    fn optional(&mut self, ctx: &mut Context, value: Value) -> Value {
        if self.is_optional(ctx, value) {
            value
        } else {
            self.emit_value(ctx, |ctx| WrapOp::new(ctx, value))
        }
    }

    /// Convert `value` to the type of a typed boundary: wrap into `T | undefined`, or unwrap
    /// (throwing `TypeError` on `undefined`) where `T` is required.
    fn fit(&mut self, ctx: &mut Context, value: Value, target: TypeHandle) -> Result<Value> {
        let ty = value.get_type(ctx);
        let wanted = optional_value(ctx, target).unwrap_or(target);
        if wanted.deref(ctx).is::<perry_mir::types::NumberStringType>()
            && (ty.deref(ctx).is::<FP64Type>() || ty.deref(ctx).is::<StringType>())
        {
            let tag = if ty.deref(ctx).is::<FP64Type>() {
                "number"
            } else {
                "string"
            };
            let boxed = self.emit_value(ctx, |ctx| {
                perry_mir::variants::VariantNewOp::new(ctx, wanted, tag, Some(value))
            });
            return self.fit(ctx, boxed, target);
        }
        if target.deref(ctx).is::<perry_mir::types::ByteInputType>() {
            let convert = if ty.deref(ctx).is::<StringType>() {
                Some(Intrinsic::StringByteInput)
            } else if ty.deref(ctx).is::<BytesType>() {
                Some(Intrinsic::BytesByteInput)
            } else {
                None
            };
            if let Some(convert) = convert {
                return Ok(self
                    .intrinsic(ctx, convert, vec![value])
                    .expect("byte input"));
            }
        }
        if ty.deref(ctx).is::<JsonType>() && ty != target {
            if let Some(states) = absence(ctx, target) {
                let inner = optional_value(ctx, target).expect("optional");
                let convert = |b: &mut Self, ctx: &mut Context| {
                    let converted = b.fit(ctx, value, inner)?;
                    b.fit(ctx, converted, target)
                };
                let null_or_value = |b: &mut Self, ctx: &mut Context| {
                    if states.admits_null() {
                        let null = b
                            .intrinsic(ctx, Intrinsic::JsonIsNull, vec![value])
                            .expect("boolean");
                        b.choose(
                            ctx,
                            target,
                            null,
                            |b, ctx| Ok(b.emit_value(ctx, |ctx| NullOp::new(ctx, target))),
                            convert,
                        )
                    } else {
                        convert(b, ctx)
                    }
                };
                return if states.admits_undefined() {
                    let undefined = self
                        .intrinsic(ctx, Intrinsic::JsonIsUndefined, vec![value])
                        .expect("boolean");
                    self.choose(
                        ctx,
                        target,
                        undefined,
                        |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, target))),
                        null_or_value,
                    )
                } else {
                    null_or_value(self, ctx)
                };
            }
            let intrinsic = if target.deref(ctx).is::<FP64Type>() {
                Intrinsic::JsonRequireNumber
            } else if target.deref(ctx).is::<StringType>() {
                Intrinsic::JsonToString
            } else if self.is_bool_type(ctx, target) {
                Intrinsic::JsonToBool
            } else {
                return self.decode_aggregate(ctx, value, target);
            };
            return Ok(self.intrinsic(ctx, intrinsic, vec![value]).expect("value"));
        }
        let thrown = optional_value(ctx, ty).unwrap_or(ty);
        let wanted = optional_value(ctx, target).unwrap_or(target);
        if thrown.deref(ctx).is::<ExceptionType>() && thrown != wanted {
            // A thrown value (a rejection's `reason`) used as a number or string must be one.
            let intrinsic = if wanted.deref(ctx).is::<FP64Type>() {
                Intrinsic::ExceptionToNumber
            } else if wanted.deref(ctx).is::<StringType>() {
                Intrinsic::ExceptionToString
            } else {
                bail!("thrown values can only be used as numbers and strings yet")
            };
            let exception = self.defined(ctx, value)?;
            let converted = self
                .intrinsic(ctx, intrinsic, vec![exception])
                .expect("value");
            return self.fit(ctx, converted, target);
        }
        if ty == target {
            Ok(value)
        } else if optional_value(ctx, target) == Some(ty) {
            Ok(self.emit_value(ctx, |ctx| WrapOp::with_type(ctx, value, target)))
        } else if optional_value(ctx, target).is_some()
            && optional_value(ctx, target) == optional_value(ctx, ty)
        {
            Ok(self.emit_value(ctx, |ctx| OptionalCastOp::new(ctx, value, target)))
        } else if optional_value(ctx, ty) == Some(target) {
            self.defined(ctx, value)
        } else if perry_mir::views::can_widen(ctx, ty, target) {
            Ok(self.emit_value(ctx, |ctx| {
                perry_mir::views::AggregateViewOp::new(ctx, value, target)
            }))
        } else {
            bail!(
                "value type {} differs from the expected type {}",
                ty.disp(ctx),
                target.disp(ctx)
            )
        }
    }

    /// `array[index] = value`; computed keys on records are rejected (subset rule R1).
    fn set_index(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        index: &Expr,
        value: &Expr,
    ) -> Result<Value> {
        let array = self.expression(ctx, object)?;
        let array = self.defined(ctx, array)?;
        ensure!(
            kind(ctx, array)? == Kind::Array,
            "computed property keys are only supported on arrays (subset rule R1)"
        );
        let element = self.array_element(ctx, array)?;
        let index = self.index(ctx, index)?;
        let value = self.expression_with(ctx, value, Some(element))?;
        let value = self.fit(ctx, value, element)?;
        self.emit(ctx, |ctx| ArraySetOp::new(ctx, array, index, value));
        Ok(value)
    }

    fn set_field(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        field: &str,
        value: &Expr,
    ) -> Result<Value> {
        let record = self.expression(ctx, object)?;
        let record = self.defined(ctx, record)?;
        ensure!(
            kind(ctx, record)? == Kind::Record,
            "property writes are only supported on records so far"
        );
        let field = field.to_string();
        let field_type = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected a record"))?
            .deref(ctx)
            .field(&field)
            .map(|(_, ty)| ty)
            .ok_or_else(|| {
                anyhow!("property {field} does not exist on this shape (subset rule R1)")
            })?;
        let value = self.expression_with(ctx, value, Some(field_type))?;
        let value = self.fit(ctx, value, field_type)?;
        self.emit(ctx, |ctx| FieldSetOp::new(ctx, record, field, value));
        Ok(value)
    }

    /// `{ a: …, b: … }`: evaluate fields in source order, allocate in record order.
    fn record_literal(
        &mut self,
        ctx: &mut Context,
        names: &[String],
        args: &[Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        if let Some(ty) = expected.map(|ty| optional_value(ctx, ty).unwrap_or(ty))
            && ty.deref(ctx).is::<perry_mir::types::VariantType>()
        {
            return self.variant_literal(ctx, ty, names, args);
        }
        let mut wanted: Vec<&str> = names.iter().map(String::as_str).collect();
        wanted.sort_unstable();
        // A record where `T | undefined` is expected is a `T`.
        let expected = expected
            .map(|ty| optional_value(ctx, ty).unwrap_or(ty))
            .and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
            .filter(|record| {
                record.deref(ctx).fields().is_ok_and(|fields| {
                    wanted
                        .iter()
                        .all(|name| fields.iter().any(|(field, _)| field.as_str() == *name))
                        && fields.iter().all(|(name, ty)| {
                            wanted.contains(&name.as_str())
                                || absence(ctx, *ty).is_some_and(|states| states.admits_undefined())
                        })
                })
            });
        let mut values = Vec::with_capacity(args.len());
        for (name, arg) in names.iter().zip(args) {
            let field_type = match expected {
                Some(record) => record.deref(ctx).field(name).map(|(_, ty)| ty),
                _ => None,
            };
            let value = self.expression_with(ctx, arg, field_type)?;
            let value = match field_type {
                Some(field_type) => self.fit(ctx, value, field_type)?,
                None => value,
            };
            values.push((name.clone(), value));
        }
        // With a matching expected type the fields were fitted to it; that includes recursive
        // records, which no literal's own structure can name.
        let record = match expected {
            Some(record) => {
                let fields = record
                    .deref(ctx)
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?
                    .to_vec();
                for (name, ty) in fields {
                    if !names.iter().any(|n| n == &name) {
                        let value = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty));
                        values.push((name.to_string(), value));
                    }
                }
                record
            }
            None => {
                let fields = values
                    .iter()
                    .map(|(n, v)| (n.clone(), v.get_type(ctx)))
                    .collect();
                self.shapes.mir_record(ctx, fields)?
            }
        };
        values.sort_by_key(|(name, _)| record.deref(ctx).field(name).expect("field").0);
        let operands = values.into_iter().map(|(_, v)| v).collect();
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, operands)))
    }

    fn intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Option<Value> {
        let op = self.emit(ctx, |ctx| IntrinsicOp::new(ctx, intrinsic, args));
        let operation = op.get_operation().deref(ctx);
        (operation.get_num_results() == 1).then(|| operation.get_result(0))
    }

    /// Pick `then` or `otherwise` (strings) by `condition`, through a slot.
    fn choose_string(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        otherwise: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
    ) -> Result<Value> {
        let ty = StringType::get(ctx).into();
        self.choose(ctx, ty, condition, then, otherwise)
    }

    /// A value of type `ty` from one of two branches.
    fn choose(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        condition: Value,
        then: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        otherwise: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
    ) -> Result<Value> {
        let slot = self.entry_slot(ctx, ty);
        let then_block = self.new_block(ctx, "then");
        let else_block = self.new_block(ctx, "else");
        let join = self.new_block(ctx, "join");
        self.branch(ctx, condition, then_block, else_block);
        self.switch_to(then_block);
        let value = then(self, ctx)?;
        self.store(ctx, slot, value);
        self.jump(ctx, join);
        self.switch_to(else_block);
        let value = otherwise(self, ctx)?;
        self.store(ctx, slot, value);
        self.jump(ctx, join);
        self.switch_to(join);
        self.load(ctx, slot)
    }

    /// ECMAScript `ToString` of a primitive, as `console.log` and `+` display it.
    fn display(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if let Some(inner) = optional_value(ctx, value.get_type(ctx)) {
            if inner.deref(ctx).is::<StringType>() {
                return Ok(self.emit_value(ctx, |ctx| ToStringOp::new(ctx, value)));
            }
            let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            return self.choose_string(
                ctx,
                undefined,
                |b, ctx| Ok(b.string(ctx, "undefined")),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                    b.choose_string(
                        ctx,
                        null,
                        |b, ctx| Ok(b.string(ctx, "null")),
                        |b, ctx| {
                            let op = UnwrapOp::new(ctx, value)?;
                            let defined = b.append_value(ctx, &op);
                            b.display(ctx, defined)
                        },
                    )
                },
            );
        }
        Ok(match kind(ctx, value)? {
            Kind::NumberString => {
                let tag = self.emit_value(ctx, |ctx| {
                    perry_mir::variants::VariantTagOp::new(ctx, value)
                });
                let number = self.string(ctx, "number");
                let is_number = self.compare(ctx, CompareKind::Eq, tag, number);
                self.choose_string(
                    ctx,
                    is_number,
                    |b, ctx| {
                        let ty = FP64Type::get(ctx).into();
                        let payload = b.emit_value(ctx, |ctx| {
                            perry_mir::variants::VariantPayloadOp::new(ctx, value, ty)
                        });
                        b.display(ctx, payload)
                    },
                    |b, ctx| {
                        let ty = StringType::get(ctx).into();
                        Ok(b.emit_value(ctx, |ctx| {
                            perry_mir::variants::VariantPayloadOp::new(ctx, value, ty)
                        }))
                    },
                )?
            }
            Kind::BigInt => self
                .intrinsic(ctx, Intrinsic::BigIntToString, vec![value])
                .expect("string"),
            Kind::String => value,
            Kind::Number => self
                .intrinsic(ctx, Intrinsic::NumberToString, vec![value])
                .expect("string"),
            Kind::Boolean => self.choose_string(
                ctx,
                value,
                |b, ctx| Ok(b.string(ctx, "true")),
                |b, ctx| Ok(b.string(ctx, "false")),
            )?,
            _ => bail!("only strings, numbers and booleans can be converted to strings yet"),
        })
    }

    /// `console.log(a, b, …)`: the arguments' strings joined by spaces.
    fn console_line(&mut self, ctx: &mut Context, args: &[Expr]) -> Result<Value> {
        let mut line: Option<Value> = None;
        for arg in args {
            let value = self.expression(ctx, arg)?;
            let text = self.display(ctx, value)?;
            line = Some(match line {
                None => text,
                Some(line) => {
                    let space = self.string(ctx, " ");
                    let spaced = self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, line, space));
                    self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, spaced, text))
                }
            });
        }
        Ok(match line {
            Some(line) => line,
            None => self.string(ctx, ""),
        })
    }

    fn call(&mut self, ctx: &mut Context, callee: &Expr, args: &[Expr]) -> Result<Option<Value>> {
        if let Expr::Closure { params, body, .. } = callee
            && let [param] = params.as_slice()
            && param.name == "__perry_obj_iife"
        {
            ensure!(
                matches!(args, [Expr::Object(fields)] if fields.is_empty()),
                "object literals with methods are not supported yet"
            );
            return self.object_spread(ctx, param.id, body).map(Some);
        }
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
            && matches!(**object, Expr::GlobalGet(_))
        {
            let intrinsic = match property.as_str() {
                "log" | "info" | "debug" => Intrinsic::PrintOut,
                "error" | "warn" => Intrinsic::PrintErr,
                other => bail!("console.{other} is not supported yet"),
            };
            let line = self.console_line(ctx, args)?;
            self.intrinsic(ctx, intrinsic, vec![line]);
            return Ok(None);
        }
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            if let Expr::PropertyGet {
                object: global,
                property: class,
                ..
            } = &**object
                && matches!(**global, Expr::GlobalGet(_))
                && class == "Promise"
            {
                if let ("resolve" | "reject", [value]) = (property.as_str(), args) {
                    return self.promise_settled(ctx, property, value, None).map(Some);
                }
                let combinator = match property.as_str() {
                    "all" => Combinator::All,
                    "race" => Combinator::Race,
                    "allSettled" => Combinator::AllSettled,
                    other => bail!("Promise.{other} is not supported yet"),
                };
                let [inputs] = args else {
                    bail!("Promise.{property} takes one array");
                };
                let inputs = match inputs {
                    Expr::Array(items) => self.combinator_inputs(ctx, items)?,
                    inputs => self.expression(ctx, inputs)?,
                };
                let op = PromiseCombinatorOp::new(ctx, combinator, inputs)
                    .context("Promise combinators take an array of promises of one type")?;
                return Ok(Some(self.append_value(ctx, &op)));
            }
            return self.method(ctx, object, property, args);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && name == "js_process_exit_code_set"
        {
            ensure!(args.len() == 1, "process.exitCode takes one value");
            let code = self.expression(ctx, &args[0])?;
            let code = self.arithmetic(ctx, code);
            ensure!(
                kind(ctx, code)? == Kind::Number,
                "process.exitCode must be a number"
            );
            self.intrinsic(ctx, Intrinsic::ExitCodeSet, vec![code]);
            return Ok(None);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && name == "js_native_module_named_esm_export_value"
        {
            // Module initialization of a named built-in import (`Writable`): nothing to run.
            return Ok(None);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && let Some(builtin) = self.builtins.get(name)
        {
            return match builtin {
                Builtin::SetTimeout => self.timer(ctx, args).map(Some),
                Builtin::Writable => bail!("Writable is a class; only Writable.toWeb is supported"),
                Builtin::Serve(intrinsic) => {
                    let mut operands = Vec::with_capacity(args.len());
                    for arg in args {
                        let value = self.expression(ctx, arg)?;
                        operands.push(self.defined(ctx, value)?);
                    }
                    Ok(Some(match self.intrinsic(ctx, *intrinsic, operands) {
                        Some(value) => value,
                        None => {
                            let number = FP64Type::get(ctx).into();
                            self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number))
                        }
                    }))
                }
                fs => self.fs(ctx, *fs, args).map(Some),
            };
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && let Some((binding, signature)) = self.shapes.foreign_function(name)
        {
            ensure!(
                args.len() == signature.params.len(),
                "WIT import {binding} expects {} arguments",
                signature.params.len()
            );
            let mut values = Vec::new();
            for (arg, (_, ty, _)) in args.iter().zip(&signature.params) {
                let ty = self.shapes.value_type(ctx, ty)?;
                let value = self.expression_with(ctx, arg, Some(ty))?;
                let actual = value.get_type(ctx);
                let value = if foreign::accepts_input(ctx, ty, actual) {
                    value
                } else if optional_value(ctx, ty).is_none()
                    && optional_value(ctx, actual)
                        .is_some_and(|inner| foreign::accepts_input(ctx, ty, inner))
                {
                    self.defined(ctx, value)?
                } else if optional_value(ctx, actual).is_none()
                    && optional_value(ctx, ty)
                        .is_some_and(|inner| foreign::accepts_input(ctx, inner, actual))
                {
                    let wrapped = perry_mir::types::OptionalType::get_with(
                        ctx,
                        actual,
                        absence(ctx, ty).expect("optional"),
                    )
                    .into();
                    self.emit_value(ctx, |ctx| WrapOp::with_type(ctx, value, wrapped))
                } else {
                    self.fit(ctx, value, ty)?
                };
                values.push(value);
            }
            let results = if *signature.return_type == HirType::Void {
                vec![]
            } else {
                vec![self.shapes.value_type(ctx, &signature.return_type)?]
            };
            if signature.is_async {
                let payload = self.shapes.settled_type(ctx, &signature.return_type)?;
                let params = values.iter().map(|v| v.get_type(ctx)).collect();
                let wrapper = foreign::wrapper(ctx, self.shapes, &binding, params, payload)?;
                let op = AsyncCallOp::new(ctx, wrapper, values, payload);
                return Ok(Some(self.append_value(ctx, &op)));
            }
            let op = perry_mir::foreign::ForeignOp::new(
                ctx,
                perry_mir::foreign::Phase::Call,
                &binding,
                values,
                results,
            );
            self.append(ctx, &op);
            return Ok(op.get_operation().deref(ctx).results().next());
        }
        let Expr::FuncRef(id) = callee else {
            bail!("only direct calls to module functions are supported yet");
        };
        let signature = self
            .signatures
            .get(id)
            .context("call to an unknown function")?;
        let (name, params, results) = (
            signature.name.clone(),
            signature.params.clone(),
            signature.results.clone(),
        );
        ensure!(
            args.len() == params.len(),
            "call to {name} with {} arguments, expected {}",
            args.len(),
            params.len()
        );
        let values = args
            .iter()
            .zip(&params)
            .map(|(arg, param)| {
                let value = self.expression_with(ctx, arg, Some(*param))?;
                self.fit(ctx, value, *param)
            })
            .collect::<Result<Vec<_>>>()?;
        if signature.is_async {
            let value = results[0];
            let promise = self.emit_value(ctx, |ctx| AsyncCallOp::new(ctx, name, values, value));
            return Ok(Some(promise));
        }
        let op = CallOp::new(ctx, name, values, results);
        self.append(ctx, &op);
        let operation = op.get_operation().deref(ctx);
        Ok((operation.get_num_results() == 1).then(|| operation.get_result(0)))
    }
}

/// Accepted deviation: JSON revivers, replacers and indentation are diagnosed at compile time.
const JSON_CALLBACKS: &str = "JSON revivers, replacers and indentation are not supported (accepted profile): parse into a typed shape and transform it in code";

fn is_utf8(encoding: &str) -> bool {
    matches!(encoding, "utf8" | "utf-8" | "UTF-8" | "UTF8")
}

fn statement_name(statement: &Stmt) -> String {
    format!("{statement:?}")
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or("?")
        .to_string()
}

fn expression_name(expr: &Expr) -> String {
    format!("{expr:?}")
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or("?")
        .to_string()
}

/// `Promise.<name>(argument)`: the name and the argument.
fn promise_call(expr: &Expr) -> Option<(&str, &Expr)> {
    let Expr::Call { callee, args, .. } = expr else {
        return None;
    };
    let Expr::PropertyGet {
        object, property, ..
    } = &**callee
    else {
        return None;
    };
    let Expr::PropertyGet {
        object: global,
        property: class,
        ..
    } = &**object
    else {
        return None;
    };
    let [argument] = args.as_slice() else {
        return None;
    };
    (matches!(**global, Expr::GlobalGet(_)) && class == "Promise")
        .then_some((property.as_str(), argument))
}

#[cfg(test)]
mod tests;
