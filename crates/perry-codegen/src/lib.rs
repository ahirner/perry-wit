//! perry MIR → pliron `wasm` dialect → core Wasm (`TODOs.md` P4).
//!
//! The output defines and exports linear memory, places string constants in data segments
//! from [DATA_BASE], and imports runtime functions from [HELPER_MODULE]; `perry-wit`'s linker
//! resolves those against `perry-runtime` and the helper libraries. Exports are either the raw
//! functions ([Exports::Raw], for tests) or canonical-ABI component exports
//! ([Exports::Canonical], see [boundary]). Representation decisions for this first slice:
//!
//! - `f64` stays `f64`; `i1` and `i32` become `i32`; records, arrays, strings and promises are
//!   `i32` addresses.
//! - `undefined` (R8) is the signalling-NaN bit pattern [UNDEFINED_BITS] for numbers, the null
//!   address for references and `2` for booleans. Wasm arithmetic only produces quiet NaNs, so
//!   no computed number can be mistaken for `undefined`; `perry.to_number` canonicalizes
//!   before arithmetic.
//! - Records are an `i32` shape id followed by 8-byte field slots in declaration order; arrays
//!   use the layout in [perry_runtime::array]. Exact layouts are P3.6.
//! - Every allocation is `task` tier: the builder cannot express module state yet (R4), so no
//!   value outlives its export call. Each export call is one task (see `perry_runtime::region`).
//! - Strings are UTF-8 with a cached UTF-16 length (layout in [perry_runtime::string]);
//!   constants are static objects in the data segment.
//! - Exceptions use the status convention: a function that may throw returns an extra `i32`
//!   status after its results (0 = normal, otherwise an [ErrorKind] code), and callers branch on
//!   it. The status reaches the export; P6.1 maps it to the WIT result.
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_mir::{
    attrs::{BinaryKind, CompareKind, ConvertKind, ErrorKind},
    calls::{AsyncCallOp, AwaitOp, CallOp as MirCallOp, TimerOp, is_exported},
    collections::{
        ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp, StringConcatOp, StringConstOp,
        StringLenOp,
    },
    control::{BrOp as MirBrOp, CondBrOp as MirCondBrOp, ReturnOp as MirReturnOp},
    exceptions::{
        CatchOp, CaughtOp, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExitOp, RaiseOp,
        RethrowOp,
    },
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    intrinsics::{
        Combinator, Intrinsic, IntrinsicOp, JsonStringifyOp, PromiseCombinatorOp, SettledField,
        SettledFieldOp, may_throw,
    },
    locals::UndefOp,
    optional::{IsUndefinedOp, ToNumberOp, ToStringOp, UndefinedOp, UnwrapOp, WrapOp},
    scalar::{BinaryOp as MirBinaryOp, CompareOp as MirCompareOp, ConvertOp},
    types::{
        ArrayType, BytesType, DateType, ExceptionType, HeadersType, JsonType, OptionalType,
        PromiseType, RecordType, RequestType, ResponseType, SettledType, StatsType, StringType,
        WritableType, WriterType,
    },
};
use perry_runtime::array as layout;
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
        op_interfaces::{
            BranchOpInterface, OneRegionInterface, OneResultInterface, SingleBlockRegionInterface,
            SymbolOpInterface,
        },
        ops::{ConstantOp, FuncOp, ModuleOp},
        type_interfaces::FunctionTypeInterface,
        types::{FP64Type, FunctionType, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::{Operation, verify_operation},
    printable::Printable,
    region::Region,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::APInt,
    value::Value,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::{BrOp, CondBrOp, ReturnOp},
    emit::{DataSegment, EmitOptions, Memory, emit_module},
    ops::{CallOp, ImportFuncOp, LoadOp, NumOp, SelectOp, StoreOp, set_export},
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    num::NonZeroUsize,
};

mod async_split;
pub mod boundary;

/// Import module of runtime and helper functions, resolved by `perry-wit`'s linker.
pub const HELPER_MODULE: &str = "__perry_helper";
/// First address of static data; address 0 stays null (`undefined` for references).
pub const DATA_BASE: u32 = 16;

/// How functions are exported.
pub enum Exports {
    /// Every exported TypeScript function under its own name, with the core signature of its
    /// MIR type plus the exception status, as one task per call. An async function's export
    /// drives it to completion without a host (no imports) and returns its value and status.
    /// For tests and inspection.
    Raw,
    /// Canonical-ABI exports of a WIT world, and the interfaces the world imports.
    Canonical {
        exports: Vec<boundary::Export>,
        imports: Vec<String>,
    },
}

/// The interface `setTimeout` waits on.
pub const MONOTONIC_CLOCK: &str = "wasi:clocks/monotonic-clock@0.3.0";

/// Canonical built-ins and host functions the module may import.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Host {
    ContextGet0,
    ContextSet0,
    WaitFor,
    /// `task.return` of an async export: (module, function, flat result types).
    TaskReturn(String, String, Vec<boundary::Flat>),
}

impl Host {
    fn symbol(&self) -> Identifier {
        let name = match self {
            Host::ContextGet0 => "host_context_get_0".to_string(),
            Host::ContextSet0 => "host_context_set_0".to_string(),
            Host::WaitFor => "host_wait_for".to_string(),
            Host::TaskReturn(module, function, _) => {
                let clean = |s: &str| -> String {
                    s.chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                        .collect()
                };
                format!("host_task_return_{}_{}", clean(module), clean(function))
            }
        };
        name.try_into().expect("identifier")
    }

    /// Import module and field.
    fn path(&self) -> (String, String) {
        let root = |name: &str| ("$root".to_string(), name.to_string());
        match self {
            Host::ContextGet0 => root("[context-get-0]"),
            Host::ContextSet0 => root("[context-set-0]"),
            Host::WaitFor => (
                MONOTONIC_CLOCK.to_string(),
                "[async-lower]wait-for".to_string(),
            ),
            Host::TaskReturn(module, function, _) => {
                (module.clone(), format!("[task-return]{function}"))
            }
        }
    }

    fn signature(&self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let i32 = i32_type(ctx);
        let i64: TypeHandle = IntegerType::get(ctx, 64, Signedness::Signless).into();
        match self {
            Host::ContextGet0 => (vec![], vec![i32]),
            Host::ContextSet0 => (vec![i32], vec![]),
            Host::WaitFor => (vec![i64], vec![i32]),
            Host::TaskReturn(_, _, flat) => (
                flat.iter().map(|f| boundary::flat_type(ctx, *f)).collect(),
                vec![],
            ),
        }
    }
}

/// ECMAScript `QuoteJSONString` of a key known at compile time.
fn quote_json(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    for byte in text.bytes() {
        match byte {
            b'"' => out.extend_from_slice(br#"\""#),
            b'\\' => out.extend_from_slice(br"\\"),
            0..=0x1F => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Bits of `undefined` in a `number | undefined` slot: a signalling NaN.
pub const UNDEFINED_BITS: u64 = 0x7FF0_0000_0000_0001;
/// `undefined` in a `boolean | undefined` slot.
const UNDEFINED_BOOL: i32 = 2;

/// Lower a canonicalized MIR module and encode it as a core Wasm module that still imports
/// runtime functions from [HELPER_MODULE].
pub fn compile(ctx: &mut Context, mir: &ModuleOp, exports: &Exports) -> Result<Vec<u8>> {
    let (wasm, data) = lower(ctx, mir, exports)?;
    // Resuming an `await` inside a loop enters the loop at a second block.
    for function in module_functions(ctx, &wasm) {
        pliron_wasm::reducible::make_reducible(ctx, &function);
    }
    verify_operation(wasm.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))
        .context("verifying the lowered wasm module")?;
    let end = DATA_BASE as usize + data.len();
    let options = EmitOptions {
        memory: Memory::Define {
            min_pages: end.div_ceil(65536).max(1) as u64,
            export: Some("memory".into()),
        },
        data: if data.is_empty() {
            vec![]
        } else {
            vec![DataSegment {
                offset: DATA_BASE,
                bytes: data,
            }]
        },
        names: true,
    };
    Ok(emit_module(ctx, &wasm, &options)?)
}

/// The status code of an exception kind; 0 means no exception.
pub fn status_code(kind: ErrorKind) -> i32 {
    match kind {
        ErrorKind::Any => 3,
        ErrorKind::TypeError => 1,
        ErrorKind::RangeError => 2,
        ErrorKind::Value => 4,
        ErrorKind::Exit => 5,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Runtime {
    Alloc,
    ArrayNew,
    ArrayGrow,
    F64Rem,
    TaskEnter,
    TaskResume,
    TaskSuspend,
    TaskFree,
    TaskJoin,
    TaskSubtaskEvent,
    TaskCloseSet,
    TaskResult,
    TaskSetResult,
    ReadyPop,
    CombinatorStep,
    SubtaskRegister,
    PromiseNew,
    PromiseResolve,
    PromiseReject,
    PromiseWait,
    PromiseRethrow,
    ExceptionNew,
    ExceptionRaise,
    ExceptionCatch,
    ExceptionMessage,
    Exit,
    CabiRealloc,
    StringAdopt,
    StringConcat,
    StringCompare,
    Print,
    IoPump,
    IoEvent,
    IoClose,
    ExitCodeSet,
    CommandOutcome,
    EnvGet,
    EnvKeys,
    PerformanceNow,
    DateNow,
    /// `number_format` of the number helper library.
    NumberFormat,
    /// Any other runtime or helper function: name, parameters, results.
    Call(&'static str, &'static [W], &'static [W]),
}

/// Wasm value types of [Runtime::Call] signatures.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum W {
    I32,
    I64,
    F64,
}

impl Runtime {
    fn name(self) -> &'static str {
        match self {
            Runtime::Alloc => "perry_alloc",
            Runtime::ArrayNew => "perry_array_new",
            Runtime::ArrayGrow => "perry_array_grow",
            Runtime::F64Rem => "perry_f64_rem",
            Runtime::TaskEnter => "perry_task_enter",
            Runtime::TaskResume => "perry_task_resume",
            Runtime::TaskSuspend => "perry_task_suspend",
            Runtime::TaskFree => "perry_task_free",
            Runtime::TaskJoin => "perry_task_join",
            Runtime::TaskSubtaskEvent => "perry_task_subtask_event",
            Runtime::TaskCloseSet => "perry_task_close_set",
            Runtime::TaskResult => "perry_task_result",
            Runtime::TaskSetResult => "perry_task_set_result",
            Runtime::ReadyPop => "perry_ready_pop",
            Runtime::CombinatorStep => "perry_combinator_step",
            Runtime::SubtaskRegister => "perry_subtask_register",
            Runtime::PromiseNew => "perry_promise_new",
            Runtime::PromiseResolve => "perry_promise_resolve",
            Runtime::PromiseReject => "perry_promise_reject",
            Runtime::PromiseWait => "perry_promise_wait",
            Runtime::PromiseRethrow => "perry_promise_rethrow",
            Runtime::ExceptionNew => "perry_exception_new",
            Runtime::ExceptionRaise => "perry_exception_raise",
            Runtime::ExceptionCatch => "perry_exception_catch",
            Runtime::ExceptionMessage => "perry_exception_message",
            Runtime::Exit => "perry_exit",
            Runtime::CabiRealloc => "perry_cabi_realloc",
            Runtime::StringAdopt => "perry_string_adopt",
            Runtime::StringConcat => "perry_string_concat",
            Runtime::StringCompare => "perry_string_compare",
            Runtime::Print => "perry_print",
            Runtime::IoPump => "perry_io_pump",
            Runtime::IoEvent => "perry_io_event",
            Runtime::IoClose => "perry_io_close",
            Runtime::ExitCodeSet => "perry_exit_code_set",
            Runtime::CommandOutcome => "perry_command_outcome",
            Runtime::EnvGet => "perry_env_get",
            Runtime::EnvKeys => "perry_env_keys",
            Runtime::PerformanceNow => "perry_performance_now",
            Runtime::DateNow => "perry_date_now",
            Runtime::NumberFormat => "number_format",
            Runtime::Call(name, ..) => name,
        }
    }

    fn symbol(self) -> Identifier {
        let name = self.name();
        format!("rt_{}", name.strip_prefix("perry_").unwrap_or(name))
            .try_into()
            .expect("identifier")
    }

    /// (parameters, results) as (f64?, count) pairs: all runtime values are i32 or f64.
    fn signature(self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let i32 = i32_type(ctx);
        let i64: TypeHandle = IntegerType::get(ctx, 64, Signedness::Signless).into();
        let f64: TypeHandle = FP64Type::get(ctx).into();
        match self {
            Runtime::Alloc | Runtime::ArrayNew => (vec![i32], vec![i32]),
            Runtime::ArrayGrow => (vec![i32], vec![]),
            Runtime::F64Rem => (vec![f64, f64], vec![f64]),
            Runtime::TaskFree | Runtime::TaskCloseSet | Runtime::IoPump => (vec![], vec![]),
            Runtime::TaskEnter
            | Runtime::TaskSuspend
            | Runtime::IoClose
            | Runtime::EnvKeys
            | Runtime::TaskResult
            | Runtime::ReadyPop
            | Runtime::PromiseNew => (vec![], vec![i32]),
            Runtime::TaskResume
            | Runtime::TaskJoin
            | Runtime::TaskSetResult
            | Runtime::CombinatorStep => (vec![i32], vec![]),
            Runtime::TaskSubtaskEvent => (vec![i32, i32], vec![]),
            Runtime::Print => (vec![i32, i32, i32], vec![]),
            Runtime::IoEvent => (vec![i32, i32, i32], vec![i32]),
            Runtime::ExitCodeSet => (vec![f64], vec![]),
            Runtime::CommandOutcome | Runtime::EnvGet => (vec![i32], vec![i32]),
            Runtime::PerformanceNow | Runtime::DateNow => (vec![], vec![f64]),
            Runtime::NumberFormat => (vec![f64, i32], vec![i32]),
            Runtime::PromiseRethrow
            | Runtime::ExceptionRaise
            | Runtime::ExceptionCatch
            | Runtime::ExceptionMessage => (vec![i32], vec![i32]),
            Runtime::ExceptionNew => (vec![i32, i32, i64], vec![i32]),
            Runtime::Exit => (vec![f64], vec![i32]),
            Runtime::SubtaskRegister => (vec![i32, i32, i64], vec![]),
            Runtime::PromiseResolve => (vec![i32, i64], vec![]),
            Runtime::PromiseReject | Runtime::PromiseWait => (vec![i32, i32], vec![]),
            Runtime::CabiRealloc => (vec![i32; 4], vec![i32]),
            Runtime::StringAdopt | Runtime::StringConcat | Runtime::StringCompare => {
                (vec![i32, i32], vec![i32])
            }
            Runtime::Call(_, params, results) => {
                let map = |w: &W| match w {
                    W::I32 => i32,
                    W::I64 => i64,
                    W::F64 => f64,
                };
                (
                    params.iter().map(map).collect(),
                    results.iter().map(map).collect(),
                )
            }
        }
    }
}

fn val_type_is_i64(ctx: &Context, ty: TypeHandle) -> bool {
    ty.deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|int| int.width() == 64)
}

fn i32_type(ctx: &mut Context) -> TypeHandle {
    IntegerType::get(ctx, 32, Signedness::Signless).into()
}

/// How a MIR type is represented in Wasm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Repr {
    F64,
    Bool,
    I32,
    Ref,
}

fn repr(ctx: &Context, ty: TypeHandle) -> Result<Repr> {
    let deref = ty.deref(ctx);
    if deref.is::<FP64Type>() || deref.is::<DateType>() {
        return Ok(Repr::F64);
    }
    if let Some(int) = deref.downcast_ref::<IntegerType>() {
        return match int.width() {
            1 => Ok(Repr::Bool),
            32 => Ok(Repr::I32),
            width => bail!("integers of width {width} are not supported"),
        };
    }
    if deref.is::<RecordType>()
        || deref.is::<ArrayType>()
        || deref.is::<StringType>()
        || deref.is::<PromiseType>()
        || deref.is::<SettledType>()
        || deref.is::<StatsType>()
        || deref.is::<HeadersType>()
        || deref.is::<ResponseType>()
        || deref.is::<RequestType>()
        || deref.is::<WritableType>()
        || deref.is::<WriterType>()
        || deref.is::<ExceptionType>()
        || deref.is::<BytesType>()
        || deref.is::<JsonType>()
    {
        return Ok(Repr::Ref);
    }
    if let Some(optional) = deref.downcast_ref::<OptionalType>() {
        let inner = optional.value();
        drop(deref);
        return repr(ctx, inner);
    }
    bail!("type {} has no Wasm representation", ty.disp(ctx))
}

fn wasm_type(ctx: &mut Context, ty: TypeHandle) -> Result<TypeHandle> {
    Ok(match repr(ctx, ty)? {
        Repr::F64 => FP64Type::get(ctx).into(),
        _ => i32_type(ctx),
    })
}

fn slot_kind(ctx: &Context, ty: TypeHandle) -> Result<MemKind> {
    Ok(match repr(ctx, ty)? {
        Repr::F64 => MemKind::F64,
        _ => MemKind::I32,
    })
}

fn optional_inner(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    perry_mir::optional::optional_value(ctx, ty)
}

fn module_functions(ctx: &Context, module: &ModuleOp) -> Vec<FuncOp> {
    module
        .get_body(ctx, 0)
        .deref(ctx)
        .iter(ctx)
        .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
        .collect()
}

fn function_ops(ctx: &Context, function: &FuncOp) -> Vec<Ptr<Operation>> {
    let mut ops = Vec::new();
    for block in function.get_region(ctx).deref(ctx).iter(ctx) {
        ops.extend(block.deref(ctx).iter(ctx));
    }
    ops
}

/// Functions that may complete with an exception: those with a throwing op other than a call,
/// and, to a fixpoint, those calling one of them.
fn throwing_functions(ctx: &Context, functions: &[FuncOp]) -> HashSet<Identifier> {
    let mut throws = HashSet::new();
    let mut calls: Vec<(Identifier, Vec<Identifier>)> = Vec::new();
    for function in functions {
        let name = function.get_symbol_name(ctx);
        // An async function rejects its promise instead of returning a status.
        if perry_mir::calls::is_async(ctx, function) {
            continue;
        }
        let mut callees = Vec::new();
        for op in function_ops(ctx, function) {
            if let Some(call) = Operation::get_op::<MirCallOp>(op, ctx) {
                callees.push(call.callee(ctx));
            } else if may_throw(ctx, op) {
                throws.insert(name.clone());
            }
        }
        calls.push((name, callees));
    }
    loop {
        let before = throws.len();
        for (name, callees) in &calls {
            if callees.iter().any(|callee| throws.contains(callee)) {
                throws.insert(name.clone());
            }
        }
        if throws.len() == before {
            return throws;
        }
    }
}

/// Static data placed at [DATA_BASE].
#[derive(Default)]
struct Data {
    bytes: Vec<u8>,
    strings: HashMap<String, u32>,
}

impl Data {
    /// A static string object followed by its bytes (layout of [perry_runtime::string]).
    fn string(&mut self, text: &str) -> u32 {
        if let Some(&address) = self.strings.get(text) {
            return address;
        }
        while !self.bytes.len().is_multiple_of(8) {
            self.bytes.push(0);
        }
        let address = DATA_BASE + self.bytes.len() as u32;
        let utf16 = text.encode_utf16().count() as u32;
        for word in [
            text.len() as u32,
            utf16,
            address + perry_runtime::string::SIZE,
            0,
        ] {
            self.bytes.extend_from_slice(&word.to_le_bytes());
        }
        self.bytes.extend_from_slice(text.as_bytes());
        self.strings.insert(text.to_string(), address);
        address
    }
}

struct ModuleState {
    data: Data,
    wasm: ModuleOp,
    throws: HashSet<Identifier>,
    runtime: BTreeSet<Runtime>,
    host: BTreeSet<Host>,
    /// Interfaces the WIT world imports; `None` when compiling without a world.
    imports: Option<HashSet<String>>,
    /// Async functions in dispatch-kind order.
    async_functions: Vec<Identifier>,
    shapes: HashMap<String, i32>,
    /// Whether any function writes console output (finish and callbacks then drive it).
    uses_io: bool,
    /// Whether any function starts a `node:fs` operation (callbacks pass it events).
    uses_fs: bool,
    /// Generated `JSON.stringify` serializers by MIR type.
    json_serializers: HashMap<TypeHandle, Identifier>,
}

impl ModuleState {
    fn can_import(&self, interface: &str) -> bool {
        self.imports
            .as_ref()
            .is_none_or(|imports| imports.contains(interface))
    }
}

/// Lower canonicalized MIR (after `perry_mir::pipeline::canonicalize`) to the `wasm` dialect.
pub fn lower(ctx: &mut Context, mir: &ModuleOp, exports: &Exports) -> Result<(ModuleOp, Vec<u8>)> {
    let name = mir.get_symbol_name(ctx);
    let functions = module_functions(ctx, mir);
    let async_functions: Vec<Identifier> = functions
        .iter()
        .filter(|f| perry_mir::calls::is_async(ctx, f))
        .map(|f| f.get_symbol_name(ctx))
        .collect();
    let mut state = ModuleState {
        data: Data::default(),
        wasm: ModuleOp::new(ctx, name),
        throws: throwing_functions(ctx, &functions),
        runtime: BTreeSet::new(),
        host: BTreeSet::new(),
        imports: match exports {
            Exports::Raw => None,
            Exports::Canonical { imports, .. } => Some(imports.iter().cloned().collect()),
        },
        async_functions: async_functions.clone(),
        shapes: HashMap::new(),
        uses_io: false,
        uses_fs: false,
        json_serializers: HashMap::new(),
    };
    for function in &functions {
        let symbol = function.get_symbol_name(ctx);
        if let Some(kind) = async_functions.iter().position(|f| *f == symbol) {
            async_split::lower(ctx, &mut state, function, kind as i32)
                .with_context(|| format!("lowering async function {symbol} to wasm"))?;
            if matches!(exports, Exports::Raw) && is_exported(ctx, function) {
                let wrapper = async_export_wrapper(ctx, &mut state, function, &symbol)?;
                state.wasm.append_operation(ctx, wrapper.get_operation(), 0);
            }
            continue;
        }
        let lowered = FunctionLowering::lower(ctx, &mut state, function)
            .with_context(|| format!("lowering function {symbol} to wasm"))?;
        state.wasm.append_operation(ctx, lowered.get_operation(), 0);
        if matches!(exports, Exports::Raw) && is_exported(ctx, function) {
            let wrapper = export_wrapper(ctx, &mut state, &lowered, &symbol)?;
            state.wasm.append_operation(ctx, wrapper.get_operation(), 0);
        }
    }
    if !async_functions.is_empty() {
        async_split::drive(ctx, &mut state, &async_functions);
    }
    if let Exports::Canonical { exports, .. } = exports {
        if state.uses_io
            && let Some(export) = exports.iter().find(|e| e.task_return.is_none())
        {
            bail!(
                "console output needs async exports: WASI P3 output streams are asynchronous, but {} is a synchronous export",
                export.core_name
            );
        }
        // A command reports uncaught exceptions on stderr.
        if exports.iter().any(|e| e.command) {
            state.uses_io = true;
        }
        let mir_functions: HashMap<String, FuncOp> = functions
            .iter()
            .map(|f| (f.get_symbol_name(ctx).to_string(), *f))
            .collect();
        for export in exports {
            let function = mir_functions.get(&export.function).ok_or_else(|| {
                anyhow!(
                    "WIT export {} has no function {}",
                    export.core_name,
                    export.function
                )
            })?;
            boundary::export(ctx, &mut state, function, export)
                .with_context(|| format!("binding WIT export {}", export.core_name))?;
        }
        boundary::realloc(ctx, &mut state);
    }
    for runtime in state.runtime.clone() {
        let (params, results) = runtime.signature(ctx);
        let ty = FunctionType::get(ctx, params, results);
        let import = ImportFuncOp::new(ctx, runtime.symbol(), ty, HELPER_MODULE, runtime.name());
        state.wasm.append_operation(ctx, import.get_operation(), 0);
    }
    for host in state.host.clone() {
        let (params, results) = host.signature(ctx);
        let ty = FunctionType::get(ctx, params, results);
        let (module, field) = host.path();
        let import = ImportFuncOp::new(ctx, host.symbol(), ty, &module, &field);
        state.wasm.append_operation(ctx, import.get_operation(), 0);
    }
    Ok((state.wasm, state.data.bytes))
}

/// `export_<name>`, exported as `<name>`: runs the function as one task.
fn export_wrapper(
    ctx: &mut Context,
    state: &mut ModuleState,
    inner: &FuncOp,
    name: &Identifier,
) -> Result<FuncOp> {
    let ty = TypedHandle::<FunctionType>::from_handle(inner.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (params, results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    let wrapper_name: Identifier = format!("export_{name}").try_into().expect("identifier");
    let wrapper = FuncOp::new(ctx, wrapper_name, ty);
    set_export(ctx, &wrapper, name.as_ref());
    let entry = wrapper.get_entry_block(ctx);
    let args: Vec<_> = entry.deref(ctx).arguments().collect();
    state.runtime.insert(Runtime::TaskEnter);
    state.runtime.insert(Runtime::TaskFree);
    let i32 = i32_type(ctx);
    let begin = CallOp::new(ctx, Runtime::TaskEnter.symbol(), vec![], vec![i32]);
    begin.get_operation().insert_at_back(entry, ctx);
    let call = CallOp::new(ctx, name.clone(), args, results);
    call.get_operation().insert_at_back(entry, ctx);
    let values: Vec<_> = call.get_operation().deref(ctx).results().collect();
    let end = CallOp::new(ctx, Runtime::TaskFree.symbol(), vec![], vec![]);
    end.get_operation().insert_at_back(entry, ctx);
    let ret = ReturnOp::new(ctx, values);
    ret.get_operation().insert_at_back(entry, ctx);
    for param in params {
        ensure!(
            repr(ctx, param)? != Repr::Ref,
            "export {name} takes a reference; reference arguments need the component boundary (P6.1)"
        );
    }
    Ok(wrapper)
}

/// Raw export of an async function: run it to completion without a host and return its value
/// and status (0 fulfilled, the exception code if rejected, 99 if still pending).
fn async_export_wrapper(
    ctx: &mut Context,
    state: &mut ModuleState,
    mir: &FuncOp,
    name: &Identifier,
) -> Result<FuncOp> {
    let ty = TypedHandle::<FunctionType>::from_handle(mir.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (params, results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    let wasm_params = params
        .iter()
        .map(|t| wasm_type(ctx, *t))
        .collect::<Result<Vec<_>>>()?;
    let value_ty = results[0];
    let value_wasm = wasm_type(ctx, value_ty)?;
    let i32 = i32_type(ctx);
    let fn_ty = FunctionType::get(ctx, wasm_params, vec![value_wasm, i32]);
    let wrapper = FuncOp::new(
        ctx,
        format!("export_{name}").try_into().expect("identifier"),
        fn_ty,
    );
    set_export(ctx, &wrapper, name.as_ref());
    let args: Vec<_> = wrapper
        .get_entry_block(ctx)
        .deref(ctx)
        .arguments()
        .collect();
    let mut b = FunctionLowering::glue(state, ctx, &wrapper);
    b.runtime(ctx, Runtime::TaskEnter, vec![]);
    let call = CallOp::new(ctx, name.clone(), args, vec![i32]).get_operation();
    b.insert(ctx, call);
    let promise = call.deref(ctx).get_result(0);
    let drive = CallOp::new(ctx, async_split::drive_name(), vec![], vec![]).get_operation();
    b.insert(ctx, drive);
    let settled = b.load(ctx, MemKind::I32, promise, perry_runtime::promise::STATE);
    let fulfilled_state = b.i32_const(ctx, perry_runtime::promise::FULFILLED as i32);
    let rejected_state = b.i32_const(ctx, perry_runtime::promise::REJECTED as i32);
    let fulfilled = b.num(ctx, NumOpcode::I32Eq, vec![settled, fulfilled_state]);
    let rejected = b.num(ctx, NumOpcode::I32Eq, vec![settled, rejected_state]);
    let reason = b.load(ctx, MemKind::I32, promise, perry_runtime::promise::STATUS);
    let pending = b.i32_const(ctx, 99);
    let failure = b.select(ctx, rejected, reason, pending);
    let zero = b.i32_const(ctx, 0);
    let status = b.select(ctx, fulfilled, zero, failure);
    let value = b.load(
        ctx,
        slot_kind(ctx, value_ty)?,
        promise,
        perry_runtime::promise::VALUE,
    );
    b.runtime(ctx, Runtime::TaskFree, vec![]);
    let ret = ReturnOp::new(ctx, vec![value, status]).get_operation();
    b.insert(ctx, ret);
    Ok(wrapper)
}

struct FunctionLowering<'s> {
    state: &'s mut ModuleState,
    region: Ptr<Region>,
    block: Ptr<BasicBlock>,
    values: HashMap<Value, Value>,
    blocks: HashMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
    throws: bool,
    results: Vec<TypeHandle>,
    throw_block: Option<Ptr<BasicBlock>>,
    /// While lowering an op followed by `perry.caught`: where its exception goes instead of
    /// leaving the function. The block takes the status, then the op's results (zero on the
    /// exceptional edge), so the results dominate the code after it.
    landing: Option<(Ptr<BasicBlock>, Vec<TypeHandle>)>,
    /// The frame, when lowering the step function of an async function.
    frame: Option<async_split::Frame>,
}

impl<'s> FunctionLowering<'s> {
    /// A builder appending to the entry block of `function`, which returns `results` and does
    /// not throw. Used for component glue.
    fn glue(state: &'s mut ModuleState, ctx: &Context, function: &FuncOp) -> Self {
        FunctionLowering {
            state,
            region: function.get_region(ctx),
            block: function.get_entry_block(ctx),
            values: HashMap::new(),
            blocks: HashMap::new(),
            throws: false,
            results: vec![],
            throw_block: None,
            landing: None,
            frame: None,
        }
    }

    fn lower(ctx: &mut Context, state: &'s mut ModuleState, mir: &FuncOp) -> Result<FuncOp> {
        let symbol = mir.get_symbol_name(ctx);
        let throws = state.throws.contains(&symbol);
        let ty = TypedHandle::<FunctionType>::from_handle(mir.get_type(ctx), ctx)
            .map_err(|_| anyhow!("function type"))?;
        let (mir_params, mir_results) = {
            let ty = ty.deref(ctx);
            (ty.arg_types(), ty.res_types())
        };
        let params = mir_params
            .into_iter()
            .map(|t| wasm_type(ctx, t))
            .collect::<Result<Vec<_>>>()?;
        let results = mir_results
            .into_iter()
            .map(|t| wasm_type(ctx, t))
            .collect::<Result<Vec<_>>>()?;
        let mut signature_results = results.clone();
        if throws {
            signature_results.push(i32_type(ctx));
        }
        let fn_ty = FunctionType::get(ctx, params, signature_results);
        let function = FuncOp::new(ctx, symbol, fn_ty);
        let entry = function.get_entry_block(ctx);
        let mut lowering = FunctionLowering::glue(state, ctx, &function);
        lowering.throws = throws;
        lowering.results = results;
        lowering.map_blocks(ctx, mir, Some(entry))?;
        lowering.translate(ctx, mir)?;
        Ok(function)
    }

    /// Create a Wasm block for every block of `mir` (the MIR entry maps to `entry` if given)
    /// and map block arguments. Returns the block of the MIR entry.
    fn map_blocks(
        &mut self,
        ctx: &mut Context,
        mir: &FuncOp,
        entry: Option<Ptr<BasicBlock>>,
    ) -> Result<Ptr<BasicBlock>> {
        let mir_blocks: Vec<_> = mir.get_region(ctx).deref(ctx).iter(ctx).collect();
        for (index, &mir_block) in mir_blocks.iter().enumerate() {
            let block = match entry {
                Some(entry) if index == 0 => entry,
                _ => {
                    let mir_types: Vec<_> = mir_block
                        .deref(ctx)
                        .arguments()
                        .map(|arg| arg.get_type(ctx))
                        .collect();
                    let types = mir_types
                        .into_iter()
                        .map(|t| wasm_type(ctx, t))
                        .collect::<Result<Vec<_>>>()?;
                    self.new_block(ctx, types)
                }
            };
            let pairs: Vec<_> = mir_block
                .deref(ctx)
                .arguments()
                .zip(block.deref(ctx).arguments())
                .collect();
            self.values.extend(pairs);
            self.blocks.insert(mir_block, block);
        }
        Ok(self.blocks[&mir_blocks[0]])
    }

    /// Translate the ops of every block of `mir` (after [Self::map_blocks]). In a step
    /// function, values kept in the frame are stored where they are defined.
    fn translate(&mut self, ctx: &mut Context, mir: &FuncOp) -> Result<()> {
        let mir_blocks: Vec<_> = mir.get_region(ctx).deref(ctx).iter(ctx).collect();
        for mir_block in mir_blocks {
            self.block = self.blocks[&mir_block];
            let args: Vec<_> = mir_block.deref(ctx).arguments().collect();
            self.spill(ctx, &args);
            let ops: Vec<_> = mir_block.deref(ctx).iter(ctx).collect();
            let mut index = 0;
            while index < ops.len() {
                let op = ops[index];
                // `op; status = perry.caught`: route the exception of `op` to `status`.
                let caught = ops
                    .get(index + 1)
                    .copied()
                    .filter(|next| Operation::get_op::<CaughtOp>(*next, ctx).is_some());
                if caught.is_some() {
                    let mut types = Vec::new();
                    let mir_types: Vec<_> =
                        op.deref(ctx).results().map(|r| r.get_type(ctx)).collect();
                    for ty in mir_types {
                        types.push(wasm_type(ctx, ty)?);
                    }
                    let mut args = vec![i32_type(ctx)];
                    args.extend(types.iter().copied());
                    self.landing = Some((self.new_block(ctx, args), types));
                }
                let lowered = self.op(ctx, op).with_context(|| {
                    format!("lowering {}", Operation::get_op_dyn(op, ctx).get_opid())
                });
                let landing = self.landing.take();
                lowered?;
                let results: Vec<_> = op.deref(ctx).results().collect();
                self.spill(ctx, &results);
                if let (Some(caught), Some((landing, _))) = (caught, landing) {
                    let zero = self.i32_const(ctx, 0);
                    let mut args = vec![zero];
                    for result in &results {
                        args.push(self.values[result]);
                    }
                    self.br(ctx, landing, args);
                    self.block = landing;
                    let status = landing.deref(ctx).get_argument(0);
                    for (position, result) in results.iter().enumerate() {
                        let arg = landing.deref(ctx).get_argument(position + 1);
                        self.values.insert(*result, arg);
                    }
                    self.spill(ctx, &results);
                    self.define(ctx, caught, status);
                    let results: Vec<_> = caught.deref(ctx).results().collect();
                    self.spill(ctx, &results);
                    index += 1;
                }
                index += 1;
            }
        }
        Ok(())
    }

    /// Store the frame-resident values among `values` into their slots.
    fn spill(&mut self, ctx: &mut Context, values: &[Value]) {
        let Some(frame) = &self.frame else {
            return;
        };
        let frame_value = frame.frame;
        let stores: Vec<_> = values
            .iter()
            .filter_map(|v| frame.slots.get(v).map(|slot| (*v, *slot)))
            .collect();
        for (value, (offset, kind)) in stores {
            if let Some(&wasm) = self.values.get(&value) {
                self.store(ctx, kind, frame_value, offset, wasm);
            }
        }
    }

    // --- construction helpers ---------------------------------------------------------------

    fn new_block(&mut self, ctx: &mut Context, args: Vec<TypeHandle>) -> Ptr<BasicBlock> {
        let block = BasicBlock::new(ctx, None, args);
        block.insert_at_back(self.region, ctx);
        block
    }

    fn insert(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Ptr<Operation> {
        op.insert_at_back(self.block, ctx);
        op
    }

    fn result(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Value {
        self.insert(ctx, op).deref(ctx).get_result(0)
    }

    fn i32_const(&mut self, ctx: &mut Context, value: i32) -> Value {
        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
        let attr = IntegerAttr::new(
            ty,
            APInt::from_u64(value as u32 as u64, NonZeroUsize::new(32).unwrap()),
        );
        let op = ConstantOp::new(ctx, Box::new(attr)).get_operation();
        self.result(ctx, op)
    }

    fn i64_const(&mut self, ctx: &mut Context, value: u64) -> Value {
        let ty = IntegerType::get(ctx, 64, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(value, NonZeroUsize::new(64).unwrap()));
        let op = ConstantOp::new(ctx, Box::new(attr)).get_operation();
        self.result(ctx, op)
    }

    fn f64_const(&mut self, ctx: &mut Context, value: f64) -> Value {
        let op = ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(value))).get_operation();
        self.result(ctx, op)
    }

    fn num(&mut self, ctx: &mut Context, opcode: NumOpcode, operands: Vec<Value>) -> Value {
        let op = NumOp::new(ctx, opcode, operands).get_operation();
        self.result(ctx, op)
    }

    fn select(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: Value,
        otherwise: Value,
    ) -> Value {
        let op = SelectOp::new(ctx, condition, then, otherwise).get_operation();
        self.result(ctx, op)
    }

    fn load(&mut self, ctx: &mut Context, kind: MemKind, address: Value, offset: u32) -> Value {
        let op = LoadOp::new(ctx, kind, address, offset).get_operation();
        self.result(ctx, op)
    }

    fn store(
        &mut self,
        ctx: &mut Context,
        kind: MemKind,
        address: Value,
        offset: u32,
        value: Value,
    ) {
        let op = StoreOp::new(ctx, kind, address, offset, value).get_operation();
        self.insert(ctx, op);
    }

    fn runtime(&mut self, ctx: &mut Context, function: Runtime, args: Vec<Value>) -> Option<Value> {
        self.state.runtime.insert(function);
        let (_, results) = function.signature(ctx);
        let op = CallOp::new(ctx, function.symbol(), args, results).get_operation();
        self.insert(ctx, op);
        op.deref(ctx).results().next()
    }

    fn br(&mut self, ctx: &mut Context, dest: Ptr<BasicBlock>, args: Vec<Value>) {
        let op = BrOp::new(ctx, dest, args).get_operation();
        self.insert(ctx, op);
    }

    fn cond_br(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: (Ptr<BasicBlock>, Vec<Value>),
        otherwise: (Ptr<BasicBlock>, Vec<Value>),
    ) {
        let op =
            CondBrOp::new(ctx, condition, then.0, then.1, otherwise.0, otherwise.1).get_operation();
        self.insert(ctx, op);
    }

    /// The Wasm value of MIR `value`; frame-resident values are loaded from the frame.
    fn mapped(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if let Some(frame) = &self.frame
            && let Some(&(offset, kind)) = frame.slots.get(&value)
        {
            let frame = frame.frame;
            return Ok(self.load(ctx, kind, frame, offset));
        }
        self.values
            .get(&value)
            .copied()
            .ok_or_else(|| anyhow!("operand defined after its use"))
    }

    fn operand(&mut self, ctx: &mut Context, op: Ptr<Operation>, index: usize) -> Result<Value> {
        let value = op.deref(ctx).get_operand(index);
        self.mapped(ctx, value)
    }

    fn operands(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<Vec<Value>> {
        let operands: Vec<_> = op.deref(ctx).operands().collect();
        operands.into_iter().map(|v| self.mapped(ctx, v)).collect()
    }

    /// Call a canonical built-in or host import.
    fn host(&mut self, ctx: &mut Context, host: Host, args: Vec<Value>) -> Option<Value> {
        let (_, results) = host.signature(ctx);
        let symbol = host.symbol();
        self.state.host.insert(host);
        let op = CallOp::new(ctx, symbol, args, results).get_operation();
        self.insert(ctx, op);
        op.deref(ctx).results().next()
    }

    fn define(&mut self, ctx: &Context, op: Ptr<Operation>, value: Value) {
        self.values.insert(op.deref(ctx).get_result(0), value);
    }

    // --- exceptions ---------------------------------------------------------------------------

    /// The block that returns zero results with the status in its argument.
    fn throw_block(&mut self, ctx: &mut Context) -> Ptr<BasicBlock> {
        if let Some(block) = self.throw_block {
            return block;
        }
        if self.frame.is_some() {
            let block = self.reject_block(ctx);
            self.throw_block = Some(block);
            return block;
        }
        let i32 = i32_type(ctx);
        let block = self.new_block(ctx, vec![i32]);
        let saved = std::mem::replace(&mut self.block, block);
        let mut values = Vec::new();
        for ty in self.results.clone() {
            let zero = if ty.deref(ctx).is::<FP64Type>() {
                self.f64_const(ctx, 0.0)
            } else {
                self.i32_const(ctx, 0)
            };
            values.push(zero);
        }
        values.push(block.deref(ctx).get_argument(0));
        let op = ReturnOp::new(ctx, values).get_operation();
        self.insert(ctx, op);
        self.block = saved;
        self.throw_block = Some(block);
        block
    }

    /// Where an exception goes: the landing of an op followed by `perry.caught`, or out of the
    /// function.
    fn throw_target(&mut self, ctx: &mut Context, status: Value) -> (Ptr<BasicBlock>, Vec<Value>) {
        match self.landing.clone() {
            Some((landing, types)) => {
                let mut args = vec![status];
                for ty in types {
                    args.push(if ty.deref(ctx).is::<FP64Type>() {
                        self.f64_const(ctx, 0.0)
                    } else if val_type_is_i64(ctx, ty) {
                        self.i64_const(ctx, 0)
                    } else {
                        self.i32_const(ctx, 0)
                    });
                }
                (landing, args)
            }
            None => (self.throw_block(ctx), vec![status]),
        }
    }

    /// Throw `status` if `failed` is non-zero; code after this runs on the normal path.
    fn throw_if(&mut self, ctx: &mut Context, failed: Value, kind: ErrorKind) {
        let status = self.i32_const(ctx, status_code(kind));
        self.throw_status_if(ctx, failed, status);
    }

    /// Throw with `status` if `failed` is non-zero.
    fn throw_status_if(&mut self, ctx: &mut Context, failed: Value, status: Value) {
        debug_assert!(
            self.throws,
            "throwing op in a function classified as non-throwing"
        );
        let throw = self.throw_target(ctx, status);
        let next = self.new_block(ctx, vec![]);
        self.cond_br(ctx, failed, throw, (next, vec![]));
        self.block = next;
    }

    // --- optional values ----------------------------------------------------------------------

    fn undefined(&mut self, ctx: &mut Context, inner: TypeHandle) -> Result<Value> {
        Ok(match repr(ctx, inner)? {
            Repr::F64 => {
                let bits = self.i64_const(ctx, UNDEFINED_BITS);
                self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits])
            }
            Repr::Bool => self.i32_const(ctx, UNDEFINED_BOOL),
            Repr::Ref => self.i32_const(ctx, 0),
            Repr::I32 => bail!("i32 | undefined has no representation"),
        })
    }

    fn is_undefined(
        &mut self,
        ctx: &mut Context,
        value: Value,
        inner: TypeHandle,
    ) -> Result<Value> {
        Ok(match repr(ctx, inner)? {
            Repr::F64 => {
                let bits = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
                let undefined = self.i64_const(ctx, UNDEFINED_BITS);
                self.num(ctx, NumOpcode::I64Eq, vec![bits, undefined])
            }
            Repr::Bool => {
                let undefined = self.i32_const(ctx, UNDEFINED_BOOL);
                self.num(ctx, NumOpcode::I32Eq, vec![value, undefined])
            }
            Repr::Ref => self.num(ctx, NumOpcode::I32Eqz, vec![value]),
            Repr::I32 => bail!("i32 | undefined has no representation"),
        })
    }

    // --- arrays -------------------------------------------------------------------------------

    /// Element address and validity for `index` (f64 or i32) in an array of length `len`.
    fn element_index(
        &mut self,
        ctx: &mut Context,
        index: Value,
        len: Value,
    ) -> Result<(Value, Value, Option<Value>)> {
        if index.get_type(ctx).deref(ctx).is::<FP64Type>() {
            let i = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![index]);
            let back = self.num(ctx, NumOpcode::F64ConvertI32U, vec![i]);
            let integral = self.num(ctx, NumOpcode::F64Eq, vec![back, index]);
            let below = self.num(ctx, NumOpcode::I32LtU, vec![i, len]);
            let ok = self.num(ctx, NumOpcode::I32And, vec![integral, below]);
            Ok((i, ok, Some(integral)))
        } else {
            let ok = self.num(ctx, NumOpcode::I32LtU, vec![index, len]);
            Ok((index, ok, None))
        }
    }

    fn slot_address(&mut self, ctx: &mut Context, data: Value, index: Value) -> Value {
        let three = self.i32_const(ctx, 3);
        let offset = self.num(ctx, NumOpcode::I32Shl, vec![index, three]);
        self.num(ctx, NumOpcode::I32Add, vec![data, offset])
    }

    /// Append `value` to `array`, growing it through the runtime when full.
    fn push(&mut self, ctx: &mut Context, array: Value, value: Value, kind: MemKind) {
        let len = self.load(ctx, MemKind::I32, array, layout::LEN);
        let cap = self.load(ctx, MemKind::I32, array, layout::CAP);
        let full = self.num(ctx, NumOpcode::I32Eq, vec![len, cap]);
        let grow = self.new_block(ctx, vec![]);
        let write = self.new_block(ctx, vec![]);
        self.cond_br(ctx, full, (grow, vec![]), (write, vec![]));
        self.block = grow;
        self.runtime(ctx, Runtime::ArrayGrow, vec![array]);
        self.br(ctx, write, vec![]);
        self.block = write;
        let data = self.load(ctx, MemKind::I32, array, layout::DATA);
        let address = self.slot_address(ctx, data, len);
        self.store(ctx, kind, address, 0, value);
        let one = self.i32_const(ctx, 1);
        let next = self.num(ctx, NumOpcode::I32Add, vec![len, one]);
        self.store(ctx, MemKind::I32, array, layout::LEN, next);
    }

    fn element_type(ctx: &Context, array: Value) -> Result<TypeHandle> {
        let ty = TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected an array"))?;
        Ok(ty.deref(ctx).element())
    }

    fn field_slot(ctx: &Context, record: Value, field: &Identifier) -> Result<(u32, TypeHandle)> {
        let ty = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected a record"))?;
        let ty = ty.deref(ctx);
        let (index, field_ty) = ty
            .field(field)
            .ok_or_else(|| anyhow!("record has no field {field}"))?;
        Ok((8 + 8 * index as u32, field_ty))
    }

    // --- intrinsics ---------------------------------------------------------------------------

    fn needs_import(&self, interface: &str, what: &str) -> Result<()> {
        ensure!(
            self.state.can_import(interface),
            "{what} needs the WIT world to import {interface}"
        );
        Ok(())
    }

    fn intrinsic(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        intrinsic: Intrinsic,
    ) -> Result<()> {
        let args = self.operands(ctx, op)?;
        let value = match intrinsic {
            Intrinsic::PrintOut | Intrinsic::PrintErr => {
                let (interface, fd) = if intrinsic == Intrinsic::PrintOut {
                    ("wasi:cli/stdout@0.3.0", 0)
                } else {
                    ("wasi:cli/stderr@0.3.0", 1)
                };
                self.needs_import(interface, "console output")?;
                self.state.uses_io = true;
                let fd = self.i32_const(ctx, fd);
                let newline = self.i32_const(ctx, 1);
                self.runtime(ctx, Runtime::Print, vec![fd, args[0], newline]);
                None
            }
            Intrinsic::NumberToString => {
                // A string header followed by up to 40 bytes of ryu-js output.
                let size = self.i32_const(ctx, (perry_runtime::string::SIZE + 40) as i32);
                let string = self
                    .runtime(ctx, Runtime::Alloc, vec![size])
                    .expect("string");
                let header = self.i32_const(ctx, perry_runtime::string::SIZE as i32);
                let data = self.num(ctx, NumOpcode::I32Add, vec![string, header]);
                let len = self
                    .runtime(ctx, Runtime::NumberFormat, vec![args[0], data])
                    .expect("length");
                use perry_runtime::string::{BUFFER, DATA, LEN, UTF16};
                self.store(ctx, MemKind::I32, string, LEN, len);
                self.store(ctx, MemKind::I32, string, UTF16, len);
                self.store(ctx, MemKind::I32, string, DATA, data);
                let zero = self.i32_const(ctx, 0);
                self.store(ctx, MemKind::I32, string, BUFFER, zero);
                Some(string)
            }
            Intrinsic::EnvGet | Intrinsic::EnvKeys => {
                self.needs_import("wasi:cli/environment@0.3.0", "process.env")?;
                let function = if intrinsic == Intrinsic::EnvGet {
                    Runtime::EnvGet
                } else {
                    Runtime::EnvKeys
                };
                self.runtime(ctx, function, args)
            }
            Intrinsic::PerformanceNow => {
                self.needs_import(MONOTONIC_CLOCK, "performance.now()")?;
                self.runtime(ctx, Runtime::PerformanceNow, args)
            }
            Intrinsic::DateNow => {
                self.needs_import("wasi:clocks/system-clock@0.3.0", "Date.now()")?;
                self.runtime(ctx, Runtime::DateNow, args)
            }
            Intrinsic::ExitCodeSet => {
                self.runtime(ctx, Runtime::ExitCodeSet, args);
                None
            }
            Intrinsic::MathFloor => Some(self.num(ctx, NumOpcode::F64Floor, args)),
            Intrinsic::MathCeil => Some(self.num(ctx, NumOpcode::F64Ceil, args)),
            Intrinsic::MathTrunc => Some(self.num(ctx, NumOpcode::F64Trunc, args)),
            Intrinsic::MathAbs => Some(self.num(ctx, NumOpcode::F64Abs, args)),
            Intrinsic::MathSqrt => Some(self.num(ctx, NumOpcode::F64Sqrt, args)),
            Intrinsic::MathMin => Some(self.num(ctx, NumOpcode::F64Min, args)),
            Intrinsic::MathMax => Some(self.num(ctx, NumOpcode::F64Max, args)),
            Intrinsic::MathRound => {
                // floor(x), plus one when the fraction is at least ½; the result keeps the
                // sign of x, so -0.4 rounds to -0 as in ECMAScript.
                let x = args[0];
                let floor = self.num(ctx, NumOpcode::F64Floor, vec![x]);
                let fraction = self.num(ctx, NumOpcode::F64Sub, vec![x, floor]);
                let half = self.f64_const(ctx, 0.5);
                let up = self.num(ctx, NumOpcode::F64Ge, vec![fraction, half]);
                let one = self.f64_const(ctx, 1.0);
                let next = self.num(ctx, NumOpcode::F64Add, vec![floor, one]);
                let rounded = self.select(ctx, up, next, floor);
                Some(self.num(ctx, NumOpcode::F64Copysign, vec![rounded, x]))
            }
            Intrinsic::DateFromTime => {
                // TimeClip: NaN beyond ±8.64e15 ms, else the integer part, without -0.
                let t = args[0];
                let magnitude = self.num(ctx, NumOpcode::F64Abs, vec![t]);
                let limit = self.f64_const(ctx, 8.64e15);
                let valid = self.num(ctx, NumOpcode::F64Le, vec![magnitude, limit]);
                let truncated = self.num(ctx, NumOpcode::F64Trunc, vec![t]);
                let zero = self.f64_const(ctx, 0.0);
                let clipped = self.num(ctx, NumOpcode::F64Add, vec![truncated, zero]);
                let nan = self.f64_const(ctx, f64::NAN);
                Some(self.select(ctx, valid, clipped, nan))
            }
            Intrinsic::DateTime => Some(args[0]),
            Intrinsic::DateIso => {
                let size = self.i32_const(ctx, 32);
                let out = self.runtime(ctx, Runtime::Alloc, vec![size]).expect("out");
                let capacity = self.i32_const(ctx, 32);
                let packed = self
                    .call(
                        ctx,
                        "time_date_iso",
                        &[W::F64, W::I32, W::I32],
                        &[W::I64],
                        vec![args[0], out, capacity],
                    )
                    .expect("result");
                let thirty_two = self.i64_const(ctx, 32);
                let error = self.num(ctx, NumOpcode::I64ShrU, vec![packed, thirty_two]);
                let error = self.num(ctx, NumOpcode::I32WrapI64, vec![error]);
                let failed = self.new_block(ctx, vec![]);
                let formatted = self.new_block(ctx, vec![]);
                self.cond_br(ctx, error, (failed, vec![]), (formatted, vec![]));
                self.block = failed;
                let status = self
                    .call(ctx, "std_raise_invalid_date", &[], &[W::I32], vec![])
                    .expect("status");
                let (target, throw_args) = self.throw_target(ctx, status);
                self.br(ctx, target, throw_args);
                self.block = formatted;
                let len = self.num(ctx, NumOpcode::I32WrapI64, vec![packed]);
                self.runtime(ctx, Runtime::StringAdopt, vec![out, len])
            }
            Intrinsic::StringLower | Intrinsic::StringUpper => {
                // The text helper measures, then converts (full Unicode case mappings).
                use perry_runtime::string::{DATA, LEN};
                let string = args[0];
                let data = self.load(ctx, MemKind::I32, string, DATA);
                let len = self.load(ctx, MemKind::I32, string, LEN);
                let upper = self.i32_const(ctx, i32::from(intrinsic == Intrinsic::StringUpper));
                let zero = self.i32_const(ctx, 0);
                let sig: (&[W], &[W]) = (&[W::I32, W::I32, W::I32, W::I32], &[W::I64]);
                let measured = self
                    .call(
                        ctx,
                        "str_case_convert",
                        sig.0,
                        sig.1,
                        vec![data, len, zero, upper],
                    )
                    .expect("size");
                let bytes = self.num(ctx, NumOpcode::I32WrapI64, vec![measured]);
                let out = self.runtime(ctx, Runtime::Alloc, vec![bytes]).expect("out");
                self.call(
                    ctx,
                    "str_case_convert",
                    sig.0,
                    sig.1,
                    vec![data, len, out, upper],
                );
                self.runtime(ctx, Runtime::StringAdopt, vec![out, bytes])
            }
            Intrinsic::JsonParse => {
                use perry_runtime::string::{DATA, LEN};
                let string = args[0];
                let data = self.load(ctx, MemKind::I32, string, DATA);
                let len = self.load(ctx, MemKind::I32, string, LEN);
                let measured = self
                    .call(
                        ctx,
                        "json_measure",
                        &[W::I32, W::I32],
                        &[W::I64],
                        vec![data, len],
                    )
                    .expect("size");
                let thirty_two = self.i64_const(ctx, 32);
                let error = self.num(ctx, NumOpcode::I64ShrU, vec![measured, thirty_two]);
                let error = self.num(ctx, NumOpcode::I32WrapI64, vec![error]);
                let failed = self.new_block(ctx, vec![]);
                let valid = self.new_block(ctx, vec![]);
                self.cond_br(ctx, error, (failed, vec![]), (valid, vec![]));
                self.block = failed;
                let status = self
                    .call(ctx, "std_raise_syntax", &[], &[W::I32], vec![])
                    .expect("status");
                let (target, throw_args) = self.throw_target(ctx, status);
                self.br(ctx, target, throw_args);
                self.block = valid;
                let size = self.num(ctx, NumOpcode::I32WrapI64, vec![measured]);
                let out = self.runtime(ctx, Runtime::Alloc, vec![size]).expect("out");
                let populated = self
                    .call(
                        ctx,
                        "json_populate",
                        &[W::I32, W::I32, W::I32, W::I32],
                        &[W::I64],
                        vec![data, len, out, size],
                    )
                    .expect("root");
                Some(self.num(ctx, NumOpcode::I32WrapI64, vec![populated]))
            }
            Intrinsic::JsonIsUndefined => Some(self.num(ctx, NumOpcode::I32Eqz, args)),
            Intrinsic::MathRandom => {
                self.needs_import("wasi:random/insecure@0.3.0", "Math.random()")?;
                self.call(ctx, "std_math_random", &[], &[W::F64], args)
            }
            Intrinsic::RandomUuid => {
                self.needs_import("wasi:random/random@0.3.0", "crypto.randomUUID()")?;
                self.call(ctx, "std_random_uuid", &[], &[W::I32], args)
            }
            Intrinsic::BytesFillRandom => {
                self.needs_import("wasi:random/random@0.3.0", "crypto.getRandomValues()")?;
                self.call(ctx, "std_bytes_fill_random", &[W::I32], &[W::I32], args)
            }
            Intrinsic::JsonSerialize => {
                bail!("JSON.stringify of parsed JSON values is not supported yet")
            }
            Intrinsic::WritableStdout | Intrinsic::WritableStderr => {
                let (interface, fd) = if intrinsic == Intrinsic::WritableStdout {
                    ("wasi:cli/stdout@0.3.0", 0)
                } else {
                    ("wasi:cli/stderr@0.3.0", 1)
                };
                self.needs_import(interface, "Writable.toWeb")?;
                self.state.uses_io = true;
                let fd = self.i32_const(ctx, fd);
                self.call(ctx, "web_writable_new", &[W::I32], &[W::I32], vec![fd])
            }
            Intrinsic::FsReadText
            | Intrinsic::FsReadBytes
            | Intrinsic::FsWriteText
            | Intrinsic::FsWriteBytes
            | Intrinsic::FsStat
            | Intrinsic::FsReaddir
            | Intrinsic::FsMkdir
            | Intrinsic::FsUnlink
            | Intrinsic::FsRmdir => {
                self.needs_import("wasi:filesystem/types@0.3.0", "node:fs")?;
                self.needs_import("wasi:filesystem/preopens@0.3.0", "node:fs")?;
                self.state.uses_fs = true;
                let (name, params): (&'static str, &'static [W]) = match intrinsic {
                    Intrinsic::FsReadText => ("fs_read_text", &[W::I32]),
                    Intrinsic::FsReadBytes => ("fs_read_bytes", &[W::I32]),
                    Intrinsic::FsWriteText => ("fs_write_text", &[W::I32, W::I32]),
                    Intrinsic::FsWriteBytes => ("fs_write_bytes", &[W::I32, W::I32]),
                    Intrinsic::FsStat => ("fs_stat", &[W::I32]),
                    Intrinsic::FsReaddir => ("fs_readdir", &[W::I32]),
                    Intrinsic::FsMkdir => ("fs_mkdir", &[W::I32]),
                    Intrinsic::FsUnlink => ("fs_unlink", &[W::I32]),
                    _ => ("fs_rmdir", &[W::I32]),
                };
                self.call(ctx, name, params, &[W::I32], args)
            }
            other => {
                let (name, params, results): (&'static str, &'static [W], &'static [W]) =
                    match other {
                        Intrinsic::StringCharAt => {
                            ("std_string_char_at", &[W::I32, W::F64], &[W::I32])
                        }
                        Intrinsic::StringCharCodeAt => {
                            ("std_string_char_code_at", &[W::I32, W::F64], &[W::F64])
                        }
                        Intrinsic::StringSlice => {
                            ("std_string_slice", &[W::I32, W::F64, W::F64], &[W::I32])
                        }
                        Intrinsic::StringSubstring => {
                            ("std_string_substring", &[W::I32, W::F64, W::F64], &[W::I32])
                        }
                        Intrinsic::StringIndexOf => {
                            ("std_string_index_of", &[W::I32, W::I32, W::F64], &[W::F64])
                        }
                        Intrinsic::StringLastIndexOf => (
                            "std_string_last_index_of",
                            &[W::I32, W::I32, W::F64],
                            &[W::F64],
                        ),
                        Intrinsic::StringStartsWith => (
                            "std_string_starts_with",
                            &[W::I32, W::I32, W::F64],
                            &[W::I32],
                        ),
                        Intrinsic::StringEndsWith => {
                            ("std_string_ends_with", &[W::I32, W::I32, W::F64], &[W::I32])
                        }
                        Intrinsic::StringTrim => ("std_string_trim", &[W::I32], &[W::I32]),
                        Intrinsic::StringRepeat => {
                            ("std_string_repeat", &[W::I32, W::F64], &[W::I32])
                        }
                        Intrinsic::BytesNew => ("std_bytes_new", &[W::F64], &[W::I32]),
                        Intrinsic::BytesFromArray => ("std_bytes_from_array", &[W::I32], &[W::I32]),
                        Intrinsic::BytesCopy => ("std_bytes_copy", &[W::I32], &[W::I32]),
                        Intrinsic::BytesGet => ("std_bytes_get", &[W::I32, W::F64], &[W::I64]),
                        Intrinsic::BytesSet => ("std_bytes_set", &[W::I32, W::F64, W::F64], &[]),
                        Intrinsic::BytesLength => ("std_bytes_length", &[W::I32], &[W::F64]),
                        Intrinsic::BytesSubarray => {
                            ("std_bytes_subarray", &[W::I32, W::F64, W::F64], &[W::I32])
                        }
                        Intrinsic::BytesSlice => {
                            ("std_bytes_slice", &[W::I32, W::F64, W::F64], &[W::I32])
                        }
                        Intrinsic::BytesSetFrom => {
                            ("std_bytes_set_from", &[W::I32, W::I32, W::F64], &[])
                        }
                        Intrinsic::BytesFill => {
                            ("std_bytes_fill", &[W::I32, W::F64, W::F64, W::F64], &[])
                        }
                        Intrinsic::TextEncode => ("std_text_encode", &[W::I32], &[W::I32]),
                        Intrinsic::TextDecode => ("std_text_decode", &[W::I32, W::I32], &[W::I32]),
                        Intrinsic::JsonMember => ("std_json_member", &[W::I32, W::I32], &[W::I32]),
                        Intrinsic::JsonIndex => ("std_json_index", &[W::I32, W::F64], &[W::I32]),
                        Intrinsic::JsonLength => ("std_json_length", &[W::I32], &[W::I64]),
                        Intrinsic::JsonToNumber => ("std_json_to_number", &[W::I32], &[W::F64]),
                        Intrinsic::JsonToString => ("std_json_to_string", &[W::I32], &[W::I32]),
                        Intrinsic::JsonToBool => ("std_json_to_bool", &[W::I32], &[W::I32]),
                        Intrinsic::JsonTypeOf => ("std_json_type_of", &[W::I32], &[W::I32]),
                        Intrinsic::StatsIsFile => ("fs_stats_is_file", &[W::I32], &[W::I32]),
                        Intrinsic::StatsIsDirectory => {
                            ("fs_stats_is_directory", &[W::I32], &[W::I32])
                        }
                        Intrinsic::StatsSize => ("fs_stats_size", &[W::I32], &[W::F64]),
                        Intrinsic::HeadersNew => ("web_headers_new", &[], &[W::I32]),
                        Intrinsic::HeadersCopy => ("web_headers_copy", &[W::I32], &[W::I32]),
                        Intrinsic::HeadersAppend => {
                            ("web_headers_append", &[W::I32, W::I32, W::I32], &[])
                        }
                        Intrinsic::HeadersSet => {
                            ("web_headers_set", &[W::I32, W::I32, W::I32], &[])
                        }
                        Intrinsic::HeadersGet => ("web_headers_get", &[W::I32, W::I32], &[W::I32]),
                        Intrinsic::HeadersHas => ("web_headers_has", &[W::I32, W::I32], &[W::I32]),
                        Intrinsic::HeadersDelete => ("web_headers_delete", &[W::I32, W::I32], &[]),
                        Intrinsic::ResponseNew => (
                            "web_response_new",
                            &[W::I32, W::I32, W::F64, W::I32, W::I32],
                            &[W::I32],
                        ),
                        Intrinsic::ResponseStatus => ("web_response_status", &[W::I32], &[W::F64]),
                        Intrinsic::ResponseStatusText => {
                            ("web_response_status_text", &[W::I32], &[W::I32])
                        }
                        Intrinsic::ResponseOk => ("web_response_ok", &[W::I32], &[W::I32]),
                        Intrinsic::ResponseHeaders => {
                            ("web_response_headers", &[W::I32], &[W::I32])
                        }
                        Intrinsic::ResponseBodyUsed | Intrinsic::RequestBodyUsed => {
                            ("web_body_used", &[W::I32], &[W::I32])
                        }
                        Intrinsic::ResponseBytes | Intrinsic::RequestBytes => {
                            ("web_body_bytes", &[W::I32], &[W::I32])
                        }
                        Intrinsic::ResponseText | Intrinsic::RequestText => {
                            ("web_body_text", &[W::I32], &[W::I32])
                        }
                        Intrinsic::RequestNew => (
                            "web_request_new",
                            &[W::I32, W::I32, W::I32, W::I32, W::I32],
                            &[W::I32],
                        ),
                        Intrinsic::RequestMethod => ("web_request_method", &[W::I32], &[W::I32]),
                        Intrinsic::WritableLocked => ("web_writable_locked", &[W::I32], &[W::I32]),
                        Intrinsic::WritableGetWriter => {
                            ("web_writable_get_writer", &[W::I32], &[W::I32])
                        }
                        Intrinsic::WriterWrite => {
                            ("web_writer_write", &[W::I32, W::I32], &[W::I32])
                        }
                        Intrinsic::WriterClose => ("web_writer_close", &[W::I32], &[W::I32]),
                        Intrinsic::WriterReleaseLock => ("web_writer_release_lock", &[W::I32], &[]),
                        Intrinsic::RequestUrl => ("web_request_url", &[W::I32], &[W::I32]),
                        Intrinsic::RequestHeaders => ("web_request_headers", &[W::I32], &[W::I32]),
                        Intrinsic::ExceptionToNumber => {
                            ("std_exception_to_number", &[W::I32], &[W::F64])
                        }
                        Intrinsic::ExceptionToString => {
                            ("std_exception_to_string", &[W::I32], &[W::I32])
                        }
                        _ => unreachable!("lowered above"),
                    };
                let result = self.call(ctx, name, params, results, args);
                // Runtime results as `number | undefined` come as bits.
                result.map(|value| {
                    if results == [W::I64] {
                        self.num(ctx, NumOpcode::F64ReinterpretI64, vec![value])
                    } else {
                        value
                    }
                })
            }
        };
        if intrinsic.throws() && !matches!(intrinsic, Intrinsic::JsonParse | Intrinsic::DateIso) {
            let status = self
                .call(ctx, "perry_status", &[], &[W::I32], vec![])
                .expect("status");
            self.throw_status_if(ctx, status, status);
        }
        if let Some(value) = value {
            self.define(ctx, op, value);
        }
        Ok(())
    }

    /// Call a runtime or helper function by name.
    fn call(
        &mut self,
        ctx: &mut Context,
        name: &'static str,
        params: &'static [W],
        results: &'static [W],
        args: Vec<Value>,
    ) -> Option<Value> {
        self.runtime(ctx, Runtime::Call(name, params, results), args)
    }

    // --- JSON.stringify -----------------------------------------------------------------------

    /// The text and length of a static string, for raw writer output.
    fn static_text(&mut self, ctx: &mut Context, text: &str) -> (Value, Value) {
        let address = self.state.data.string(text) + perry_runtime::string::SIZE;
        (
            self.i32_const(ctx, address as i32),
            self.i32_const(ctx, text.len() as i32),
        )
    }

    fn json_raw(&mut self, ctx: &mut Context, writer: Value, text: &str) {
        let (data, len) = self.static_text(ctx, text);
        self.call(
            ctx,
            "std_json_raw",
            &[W::I32, W::I32, W::I32],
            &[],
            vec![writer, data, len],
        );
    }

    /// Write `value` of MIR type `ty` (defined) to the JSON writer.
    fn json_value(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        ty: TypeHandle,
    ) -> Result<()> {
        if let Some(inner) = optional_inner(ctx, ty) {
            // Inside arrays, `undefined` is written as `null`.
            let undefined = self.is_undefined(ctx, value, inner)?;
            let null = self.new_block(ctx, vec![]);
            let defined = self.new_block(ctx, vec![]);
            let join = self.new_block(ctx, vec![]);
            self.cond_br(ctx, undefined, (null, vec![]), (defined, vec![]));
            self.block = null;
            self.json_raw(ctx, writer, "null");
            self.br(ctx, join, vec![]);
            self.block = defined;
            self.json_value(ctx, writer, value, inner)?;
            self.br(ctx, join, vec![]);
            self.block = join;
            return Ok(());
        }
        let deref = ty.deref(ctx);
        if deref.is::<FP64Type>() {
            drop(deref);
            // Finite numbers in ECMAScript notation (ryu-js); NaN and infinities as null.
            let size = self.i32_const(ctx, 40);
            let scratch = self
                .runtime(ctx, Runtime::Alloc, vec![size])
                .expect("scratch");
            let difference = self.num(ctx, NumOpcode::F64Sub, vec![value, value]);
            let zero = self.f64_const(ctx, 0.0);
            let finite = self.num(ctx, NumOpcode::F64Eq, vec![difference, zero]);
            let number = self.new_block(ctx, vec![]);
            let null = self.new_block(ctx, vec![]);
            let join = self.new_block(ctx, vec![]);
            self.cond_br(ctx, finite, (number, vec![]), (null, vec![]));
            self.block = number;
            let len = self
                .runtime(ctx, Runtime::NumberFormat, vec![value, scratch])
                .expect("length");
            self.call(
                ctx,
                "std_json_raw",
                &[W::I32, W::I32, W::I32],
                &[],
                vec![writer, scratch, len],
            );
            self.br(ctx, join, vec![]);
            self.block = null;
            self.json_raw(ctx, writer, "null");
            self.br(ctx, join, vec![]);
            self.block = join;
            return Ok(());
        }
        if deref.is::<StringType>() {
            drop(deref);
            self.call(
                ctx,
                "std_json_quote",
                &[W::I32, W::I32],
                &[],
                vec![writer, value],
            );
            return Ok(());
        }
        if deref.is::<ExceptionType>() {
            drop(deref);
            self.json_raw(ctx, writer, "{}");
            return Ok(());
        }
        if deref.is::<JsonType>() {
            drop(deref);
            self.call(
                ctx,
                "std_json_value",
                &[W::I32, W::I32],
                &[],
                vec![writer, value],
            );
            return Ok(());
        }
        if deref
            .downcast_ref::<IntegerType>()
            .is_some_and(|i| i.width() == 1)
        {
            drop(deref);
            let (true_data, true_len) = self.static_text(ctx, "true");
            let (false_data, false_len) = self.static_text(ctx, "false");
            let data = self.select(ctx, value, true_data, false_data);
            let len = self.select(ctx, value, true_len, false_len);
            self.call(
                ctx,
                "std_json_raw",
                &[W::I32, W::I32, W::I32],
                &[],
                vec![writer, data, len],
            );
            return Ok(());
        }
        let container = deref.is::<RecordType>() || deref.is::<ArrayType>();
        drop(deref);
        ensure!(
            container,
            "JSON.stringify of {} is not supported yet",
            ty.disp(ctx)
        );
        let serializer = self.json_serializer(ctx, ty)?;
        let call = CallOp::new(ctx, serializer, vec![writer, value], vec![]).get_operation();
        self.insert(ctx, call);
        Ok(())
    }

    /// The serializer function of a record or array type, generated on first use (recursive
    /// types call themselves).
    fn json_serializer(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Identifier> {
        if let Some(name) = self.state.json_serializers.get(&ty) {
            return Ok(name.clone());
        }
        let name: Identifier = format!("json_{}", self.state.json_serializers.len())
            .try_into()
            .expect("identifier");
        self.state.json_serializers.insert(ty, name.clone());
        let i32 = i32_type(ctx);
        let value_ty = wasm_type(ctx, ty)?;
        let fn_ty = FunctionType::get(ctx, vec![i32, value_ty], vec![]);
        let function = FuncOp::new(ctx, name.clone(), fn_ty);
        let entry = function.get_entry_block(ctx);
        let writer = entry.deref(ctx).get_argument(0);
        let value = entry.deref(ctx).get_argument(1);
        let mut b = FunctionLowering::glue(self.state, ctx, &function);
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx).ok();
        let (open, close) = if record.is_some() {
            ('{', '}')
        } else {
            ('[', ']')
        };
        let bracket = b.i32_const(ctx, open as i32);
        let opened = b
            .call(
                ctx,
                "std_json_open",
                &[W::I32, W::I32],
                &[W::I32],
                vec![writer, bracket],
            )
            .expect("opened");
        let body = b.new_block(ctx, vec![]);
        let done = b.new_block(ctx, vec![]);
        b.cond_br(ctx, opened, (body, vec![]), (done, vec![]));
        b.block = body;
        if let Some(record) = record {
            let fields: Vec<(String, TypeHandle)> = record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .iter()
                .map(|(n, t)| (n.to_string(), *t))
                .collect();
            for (index, (field, field_ty)) in fields.into_iter().enumerate() {
                let kind = slot_kind(ctx, field_ty)?;
                let slot = b.load(ctx, kind, value, 8 + 8 * index as u32);
                let mut key = Vec::new();
                quote_json(&mut key, &field);
                key.push(b':');
                let key = String::from_utf8(key).expect("UTF-8");
                let key = b.state.data.string(&key);
                // Members holding `undefined` are left out.
                let (present, inner) = match optional_inner(ctx, field_ty) {
                    Some(inner) => {
                        let undefined = b.is_undefined(ctx, slot, inner)?;
                        let present = b.num(ctx, NumOpcode::I32Eqz, vec![undefined]);
                        (Some(present), inner)
                    }
                    None => (None, field_ty),
                };
                let member = b.new_block(ctx, vec![]);
                let next = b.new_block(ctx, vec![]);
                match present {
                    Some(present) => b.cond_br(ctx, present, (member, vec![]), (next, vec![])),
                    None => b.br(ctx, member, vec![]),
                }
                b.block = member;
                let key = b.i32_const(ctx, key as i32);
                b.call(
                    ctx,
                    "std_json_key",
                    &[W::I32, W::I32],
                    &[],
                    vec![writer, key],
                );
                b.json_value(ctx, writer, slot, inner)?;
                b.br(ctx, next, vec![]);
                b.block = next;
            }
        } else {
            let element = FunctionLowering::element_type(ctx, value).or_else(|_| {
                TypedHandle::<ArrayType>::from_handle(ty, ctx)
                    .map(|a| a.deref(ctx).element())
                    .map_err(|_| anyhow!("array"))
            })?;
            let kind = slot_kind(ctx, element)?;
            let len = b.load(ctx, MemKind::I32, value, layout::LEN);
            let data = b.load(ctx, MemKind::I32, value, layout::DATA);
            b.counted_loop(ctx, len, |b, ctx, i| {
                b.call(ctx, "std_json_item", &[W::I32], &[], vec![writer]);
                let slot = b.slot_address(ctx, data, i);
                let item = b.load(ctx, kind, slot, 0);
                b.json_value(ctx, writer, item, element)
            })?;
        }
        let bracket = b.i32_const(ctx, close as i32);
        b.call(
            ctx,
            "std_json_close",
            &[W::I32, W::I32],
            &[],
            vec![writer, bracket],
        );
        b.br(ctx, done, vec![]);
        b.block = done;
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, function.get_operation(), 0);
        Ok(name)
    }

    fn json_stringify(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        let value = self.operand(ctx, op, 0)?;
        let ty = op.deref(ctx).get_operand(0).get_type(ctx);
        let writer = self
            .call(ctx, "std_json_writer", &[], &[W::I32], vec![])
            .expect("writer");
        let json = ty.deref(ctx).is::<JsonType>();
        let text = if json || optional_inner(ctx, ty).is_some() {
            let inner = optional_inner(ctx, ty).unwrap_or(ty);
            // A top-level `undefined` stringifies to `undefined`.
            let undefined = if json {
                self.num(ctx, NumOpcode::I32Eqz, vec![value])
            } else {
                self.is_undefined(ctx, value, inner)?
            };
            let i32 = i32_type(ctx);
            let write = self.new_block(ctx, vec![]);
            let join = self.new_block(ctx, vec![i32]);
            let null = self.i32_const(ctx, 0);
            self.cond_br(ctx, undefined, (join, vec![null]), (write, vec![]));
            self.block = write;
            self.json_value(ctx, writer, value, inner)?;
            let text = self.json_finish(ctx, writer);
            self.br(ctx, join, vec![text]);
            self.block = join;
            join.deref(ctx).get_argument(0)
        } else {
            self.json_value(ctx, writer, value, ty)?;
            self.json_finish(ctx, writer)
        };
        self.define(ctx, op, text);
        Ok(())
    }

    fn json_finish(&mut self, ctx: &mut Context, writer: Value) -> Value {
        let text = self
            .call(ctx, "std_json_finish", &[W::I32], &[W::I32], vec![writer])
            .expect("text");
        let status = self
            .call(ctx, "perry_status", &[], &[W::I32], vec![])
            .expect("status");
        self.throw_status_if(ctx, status, status);
        text
    }

    // --- ops ----------------------------------------------------------------------------------

    fn op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        if let Some(constant) = Operation::get_op::<ConstantOp>(op, ctx) {
            let attr = constant
                .get_attr_builtin_constant_value(ctx)
                .ok_or_else(|| anyhow!("constant without value"))?
                .clone();
            let value = if let Some(double) = attr.downcast_ref::<FPDoubleAttr>() {
                self.f64_const(ctx, f64::from(double.clone()))
            } else if let Some(int) = attr.downcast_ref::<IntegerAttr>() {
                let bits: APInt = int.clone().into();
                self.i32_const(ctx, bits.to_u64() as u32 as i32)
            } else {
                bail!("unsupported constant")
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<UndefOp>(op, ctx).is_some() {
            let ty = op.deref(ctx).get_result(0).get_type(ctx);
            let value = match repr(ctx, ty)? {
                Repr::F64 => self.f64_const(ctx, 0.0),
                _ => self.i32_const(ctx, 0),
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(binary) = Operation::get_op::<MirBinaryOp>(op, ctx) {
            let operands = self.operands(ctx, op)?;
            let is_f64 = operands[0].get_type(ctx).deref(ctx).is::<FP64Type>();
            let value = match (binary.kind(ctx), is_f64) {
                (BinaryKind::Rem, true) => self
                    .runtime(ctx, Runtime::F64Rem, operands)
                    .expect("result"),
                (kind, true) => {
                    let opcode = match kind {
                        BinaryKind::Add => NumOpcode::F64Add,
                        BinaryKind::Sub => NumOpcode::F64Sub,
                        BinaryKind::Mul => NumOpcode::F64Mul,
                        BinaryKind::Div => NumOpcode::F64Div,
                        BinaryKind::Rem => unreachable!(),
                    };
                    self.num(ctx, opcode, operands)
                }
                (BinaryKind::Add, false) => self.num(ctx, NumOpcode::I32Add, operands),
                (BinaryKind::Sub, false) => self.num(ctx, NumOpcode::I32Sub, operands),
                (BinaryKind::Mul, false) => self.num(ctx, NumOpcode::I32Mul, operands),
                (kind, false) => bail!("i32 {kind:?} is not supported"),
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(compare) = Operation::get_op::<MirCompareOp>(op, ctx) {
            let mir_ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let operands = self.operands(ctx, op)?;
            let kind = compare.kind(ctx);
            let value = if let Some(inner) = optional_inner(ctx, mir_ty)
                && repr(ctx, inner)? == Repr::F64
            {
                // Strict equality on `number | undefined`: undefined equals only undefined.
                let (a, b) = (operands[0], operands[1]);
                let bits_a = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![a]);
                let bits_b = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![b]);
                let same = self.num(ctx, NumOpcode::I64Eq, vec![bits_a, bits_b]);
                let undefined = self.is_undefined(ctx, a, inner)?;
                let both = self.num(ctx, NumOpcode::I32And, vec![same, undefined]);
                let equal = self.num(ctx, NumOpcode::F64Eq, vec![a, b]);
                let eq = self.num(ctx, NumOpcode::I32Or, vec![both, equal]);
                match kind {
                    CompareKind::Eq => eq,
                    CompareKind::Ne => self.num(ctx, NumOpcode::I32Eqz, vec![eq]),
                    other => bail!("ordered comparison {other:?} on number | undefined"),
                }
            } else if mir_ty.deref(ctx).is::<StringType>()
                || optional_inner(ctx, mir_ty).is_some_and(|t| t.deref(ctx).is::<StringType>())
            {
                let order = self
                    .runtime(ctx, Runtime::StringCompare, operands)
                    .expect("order");
                let zero = self.i32_const(ctx, 0);
                let opcode = match kind {
                    CompareKind::Lt => NumOpcode::I32LtS,
                    CompareKind::Le => NumOpcode::I32LeS,
                    CompareKind::Gt => NumOpcode::I32GtS,
                    CompareKind::Ge => NumOpcode::I32GeS,
                    CompareKind::Eq => NumOpcode::I32Eq,
                    CompareKind::Ne => NumOpcode::I32Ne,
                };
                self.num(ctx, opcode, vec![order, zero])
            } else if operands[0].get_type(ctx).deref(ctx).is::<FP64Type>() {
                let opcode = match kind {
                    CompareKind::Lt => NumOpcode::F64Lt,
                    CompareKind::Le => NumOpcode::F64Le,
                    CompareKind::Gt => NumOpcode::F64Gt,
                    CompareKind::Ge => NumOpcode::F64Ge,
                    CompareKind::Eq => NumOpcode::F64Eq,
                    CompareKind::Ne => NumOpcode::F64Ne,
                };
                self.num(ctx, opcode, operands)
            } else {
                let opcode = match kind {
                    CompareKind::Lt => NumOpcode::I32LtS,
                    CompareKind::Le => NumOpcode::I32LeS,
                    CompareKind::Gt => NumOpcode::I32GtS,
                    CompareKind::Ge => NumOpcode::I32GeS,
                    CompareKind::Eq => NumOpcode::I32Eq,
                    CompareKind::Ne => NumOpcode::I32Ne,
                };
                self.num(ctx, opcode, operands)
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(convert) = Operation::get_op::<ConvertOp>(op, ctx) {
            let operands = self.operands(ctx, op)?;
            let opcode = match convert.kind(ctx) {
                ConvertKind::I32ToF64 => NumOpcode::F64ConvertI32S,
                ConvertKind::F64ToI32 => NumOpcode::I32TruncSatF64S,
            };
            let value = self.num(ctx, opcode, operands);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<MirBrOp>(op, ctx).is_some() {
            let dest = self.blocks[&op.deref(ctx).get_successor(0)];
            let args = self.operands(ctx, op)?;
            self.br(ctx, dest, args);
            return Ok(());
        }
        if let Some(cond_br) = Operation::get_op::<MirCondBrOp>(op, ctx) {
            let condition = self.operand(ctx, op, 0)?;
            let mut arms = Vec::new();
            for succ in 0..2 {
                let dest = self.blocks[&op.deref(ctx).get_successor(succ)];
                let mut args = Vec::new();
                for v in cond_br.successor_operands(ctx, succ) {
                    args.push(self.mapped(ctx, v)?);
                }
                arms.push((dest, args));
            }
            let otherwise = arms.pop().unwrap();
            let then = arms.pop().unwrap();
            self.cond_br(ctx, condition, then, otherwise);
            return Ok(());
        }
        if Operation::get_op::<MirReturnOp>(op, ctx).is_some() {
            let mut values = self.operands(ctx, op)?;
            if self.frame.is_some() {
                self.settle_return(ctx, values);
                return Ok(());
            }
            if self.throws {
                values.push(self.i32_const(ctx, 0));
            }
            let ret = ReturnOp::new(ctx, values).get_operation();
            self.insert(ctx, ret);
            return Ok(());
        }
        if let Some(call) = Operation::get_op::<MirCallOp>(op, ctx) {
            let callee = call.callee(ctx);
            let args = self.operands(ctx, op)?;
            let mir_results: Vec<_> = op.deref(ctx).results().collect();
            let mut results = mir_results
                .iter()
                .map(|v| v.get_type(ctx))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|t| wasm_type(ctx, t))
                .collect::<Result<Vec<_>>>()?;
            let callee_throws = self.state.throws.contains(&callee);
            if callee_throws {
                results.push(i32_type(ctx));
            }
            let wasm_call = CallOp::new(ctx, callee, args, results).get_operation();
            self.insert(ctx, wasm_call);
            let values: Vec<_> = wasm_call.deref(ctx).results().collect();
            for (mir, wasm) in mir_results.iter().zip(&values) {
                self.values.insert(*mir, *wasm);
            }
            if callee_throws {
                let status = *values.last().unwrap();
                let throw = self.throw_target(ctx, status);
                let next = self.new_block(ctx, vec![]);
                self.cond_br(ctx, status, throw, (next, vec![]));
                self.block = next;
            }
            return Ok(());
        }
        if Operation::get_op::<AllocOp>(op, ctx).is_some() {
            let record_ty = op.deref(ctx).get_result(0).get_type(ctx);
            let (name, fields) = {
                let record = TypedHandle::<RecordType>::from_handle(record_ty, ctx)
                    .map_err(|_| anyhow!("expected a record"))?;
                let record = record.deref(ctx);
                let fields: Vec<_> = record
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?
                    .iter()
                    .map(|(_, ty)| *ty)
                    .collect();
                (record.name().to_string(), fields)
            };
            let next_shape = self.state.shapes.len() as i32 + 1;
            let shape = *self.state.shapes.entry(name).or_insert(next_shape);
            let operands = self.operands(ctx, op)?;
            let size = self.i32_const(ctx, 8 + 8 * fields.len() as i32);
            let object = self
                .runtime(ctx, Runtime::Alloc, vec![size])
                .expect("address");
            let shape = self.i32_const(ctx, shape);
            self.store(ctx, MemKind::I32, object, 0, shape);
            for (index, (value, ty)) in operands.into_iter().zip(fields).enumerate() {
                let kind = slot_kind(ctx, ty)?;
                self.store(ctx, kind, object, 8 + 8 * index as u32, value);
            }
            self.define(ctx, op, object);
            return Ok(());
        }
        if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let (offset, ty) = Self::field_slot(ctx, record, &get.field(ctx))?;
            let kind = slot_kind(ctx, ty)?;
            let object = self.mapped(ctx, record)?;
            let value = self.load(ctx, kind, object, offset);
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(set) = Operation::get_op::<FieldSetOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let (offset, ty) = Self::field_slot(ctx, record, &set.field(ctx))?;
            let kind = slot_kind(ctx, ty)?;
            let operands = self.operands(ctx, op)?;
            self.store(ctx, kind, operands[0], offset, operands[1]);
            return Ok(());
        }
        if Operation::get_op::<ArrayNewOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_result(0))?;
            let kind = slot_kind(ctx, element)?;
            let operands = self.operands(ctx, op)?;
            let capacity = self.i32_const(ctx, operands.len() as i32);
            let array = self
                .runtime(ctx, Runtime::ArrayNew, vec![capacity])
                .expect("array");
            if !operands.is_empty() {
                let data = self.load(ctx, MemKind::I32, array, layout::DATA);
                for (index, value) in operands.iter().enumerate() {
                    self.store(ctx, kind, data, layout::SLOT * index as u32, *value);
                }
                let len = self.i32_const(ctx, operands.len() as i32);
                self.store(ctx, MemKind::I32, array, layout::LEN, len);
            }
            self.define(ctx, op, array);
            return Ok(());
        }
        if Operation::get_op::<ArrayLenOp>(op, ctx).is_some() {
            let array = self.operands(ctx, op)?[0];
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            self.define(ctx, op, len);
            return Ok(());
        }
        if Operation::get_op::<ArrayPushOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let kind = slot_kind(ctx, element)?;
            let operands = self.operands(ctx, op)?;
            self.push(ctx, operands[0], operands[1], kind);
            return Ok(());
        }
        if Operation::get_op::<ArrayGetOp>(op, ctx).is_some() {
            // Branch-free: read slot 0 when out of range (capacity is never 0), then select.
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let kind = slot_kind(ctx, element)?;
            let operands = self.operands(ctx, op)?;
            let (array, index) = (operands[0], operands[1]);
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, _) = self.element_index(ctx, index, len)?;
            let zero = self.i32_const(ctx, 0);
            let slot = self.select(ctx, ok, i, zero);
            let data = self.load(ctx, MemKind::I32, array, layout::DATA);
            let address = self.slot_address(ctx, data, slot);
            let loaded = self.load(ctx, kind, address, 0);
            let undefined = self.undefined(ctx, element)?;
            let value = self.select(ctx, ok, loaded, undefined);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let kind = slot_kind(ctx, element)?;
            let operands = self.operands(ctx, op)?;
            let (array, index, value) = (operands[0], operands[1], operands[2]);
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, integral) = self.element_index(ctx, index, len)?;
            let write = self.new_block(ctx, vec![]);
            let outside = self.new_block(ctx, vec![]);
            let done = self.new_block(ctx, vec![]);
            self.cond_br(ctx, ok, (write, vec![]), (outside, vec![]));
            self.block = write;
            let data = self.load(ctx, MemKind::I32, array, layout::DATA);
            let address = self.slot_address(ctx, data, i);
            self.store(ctx, kind, address, 0, value);
            self.br(ctx, done, vec![]);
            // Writing at `length` appends; any other index would make a hole.
            self.block = outside;
            let at_end = self.num(ctx, NumOpcode::I32Eq, vec![i, len]);
            let append = match integral {
                Some(integral) => self.num(ctx, NumOpcode::I32And, vec![at_end, integral]),
                None => at_end,
            };
            let hole = self.num(ctx, NumOpcode::I32Eqz, vec![append]);
            self.throw_if(ctx, hole, ErrorKind::RangeError);
            self.push(ctx, array, value, kind);
            self.br(ctx, done, vec![]);
            self.block = done;
            return Ok(());
        }
        if let Some(undefined) = Operation::get_op::<UndefinedOp>(op, ctx) {
            let ty = undefined.get_result(ctx).get_type(ctx);
            let inner = optional_inner(ctx, ty).ok_or_else(|| anyhow!("optional"))?;
            let value = self.undefined(ctx, inner)?;
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<WrapOp>(op, ctx).is_some() {
            let value = self.operands(ctx, op)?[0];
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<IsUndefinedOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let inner =
                optional_inner(ctx, mir.get_type(ctx)).ok_or_else(|| anyhow!("optional"))?;
            let value = self.mapped(ctx, mir)?;
            let result = self.is_undefined(ctx, value, inner)?;
            self.define(ctx, op, result);
            return Ok(());
        }
        if Operation::get_op::<ToNumberOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let inner =
                optional_inner(ctx, mir.get_type(ctx)).ok_or_else(|| anyhow!("optional"))?;
            let value = self.mapped(ctx, mir)?;
            let undefined = self.is_undefined(ctx, value, inner)?;
            let nan = self.f64_const(ctx, f64::NAN);
            let result = self.select(ctx, undefined, nan, value);
            self.define(ctx, op, result);
            return Ok(());
        }
        if Operation::get_op::<UnwrapOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let inner =
                optional_inner(ctx, mir.get_type(ctx)).ok_or_else(|| anyhow!("optional"))?;
            let value = self.mapped(ctx, mir)?;
            let undefined = self.is_undefined(ctx, value, inner)?;
            self.throw_if(ctx, undefined, ErrorKind::TypeError);
            // The defined value has the same representation.
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(constant) = Operation::get_op::<StringConstOp>(op, ctx) {
            let text: String = constant
                .get_attr_perry_string_value(ctx)
                .ok_or_else(|| anyhow!("string constant without value"))?
                .clone()
                .into();
            let address = self.state.data.string(&text);
            let value = self.i32_const(ctx, address as i32);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<StringConcatOp>(op, ctx).is_some() {
            let operands = self.operands(ctx, op)?;
            let value = self
                .runtime(ctx, Runtime::StringConcat, operands)
                .expect("string");
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<StringLenOp>(op, ctx).is_some() {
            let string = self.operands(ctx, op)?[0];
            let value = self.load(ctx, MemKind::I32, string, perry_runtime::string::UTF16);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<ToStringOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let inner =
                optional_inner(ctx, mir.get_type(ctx)).ok_or_else(|| anyhow!("optional"))?;
            let value = self.mapped(ctx, mir)?;
            let undefined = self.is_undefined(ctx, value, inner)?;
            let text = self.state.data.string("undefined");
            let text = self.i32_const(ctx, text as i32);
            let result = self.select(ctx, undefined, text, value);
            self.define(ctx, op, result);
            return Ok(());
        }
        if Operation::get_op::<AwaitOp>(op, ctx).is_some() {
            return self.lower_await(ctx, op);
        }
        if let Some(combinator) = Operation::get_op::<PromiseCombinatorOp>(op, ctx) {
            let name = match combinator.combinator(ctx) {
                Combinator::All => "perry_promise_all",
                Combinator::Race => "perry_promise_race",
                Combinator::AllSettled => "perry_promise_all_settled",
            };
            let inputs = self.operand(ctx, op, 0)?;
            let promise = self
                .call(ctx, name, &[W::I32], &[W::I32], vec![inputs])
                .expect("promise");
            self.define(ctx, op, promise);
            return Ok(());
        }
        if let Some(read) = Operation::get_op::<SettledFieldOp>(op, ctx) {
            // A settled view is the input promise itself (`perry_promise_all_settled`).
            use perry_runtime::promise::{FULFILLED, STATE, VALUE};
            let promise = self.operand(ctx, op, 0)?;
            let state = self.load(ctx, MemKind::I32, promise, STATE);
            let fulfilled_state = self.i32_const(ctx, FULFILLED as i32);
            let fulfilled = self.num(ctx, NumOpcode::I32Eq, vec![state, fulfilled_state]);
            let value = match read.field(ctx) {
                SettledField::Status => {
                    let yes = self.state.data.string("fulfilled");
                    let no = self.state.data.string("rejected");
                    let yes = self.i32_const(ctx, yes as i32);
                    let no = self.i32_const(ctx, no as i32);
                    self.select(ctx, fulfilled, yes, no)
                }
                field => {
                    let ty = op.deref(ctx).get_result(0).get_type(ctx);
                    let inner = optional_inner(ctx, ty).expect("optional result");
                    let loaded = self.load(ctx, slot_kind(ctx, inner)?, promise, VALUE);
                    let undefined = self.undefined(ctx, inner)?;
                    if field == SettledField::Value {
                        self.select(ctx, fulfilled, loaded, undefined)
                    } else {
                        self.select(ctx, fulfilled, undefined, loaded)
                    }
                }
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<JsonStringifyOp>(op, ctx).is_some() {
            return self.json_stringify(ctx, op);
        }
        if let Some(intrinsic) = Operation::get_op::<IntrinsicOp>(op, ctx) {
            return self.intrinsic(ctx, op, intrinsic.intrinsic(ctx).expect("verified"));
        }
        if let Some(error) = Operation::get_op::<ErrorNewOp>(op, ctx) {
            let kind = self.i32_const(ctx, status_code(error.kind(ctx)));
            let tag = self.i32_const(ctx, perry_runtime::exception::TAG_STRING as i32);
            let message = self.operand(ctx, op, 0)?;
            let message = self.num(ctx, NumOpcode::I64ExtendI32U, vec![message]);
            let value = self
                .runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, message])
                .expect("exception");
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<RaiseOp>(op, ctx).is_some() {
            let value = self.operand(ctx, op, 0)?;
            let ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let exception = if ty.deref(ctx).is::<ExceptionType>() {
                value
            } else {
                use perry_runtime::exception::{TAG_BOOLEAN, TAG_NUMBER, TAG_STRING};
                let tag = match repr(ctx, ty)? {
                    Repr::F64 => TAG_NUMBER,
                    Repr::Bool => TAG_BOOLEAN,
                    _ if ty.deref(ctx).is::<StringType>() => TAG_STRING,
                    _ => bail!("this value cannot be thrown yet"),
                };
                let kind = self.i32_const(ctx, status_code(ErrorKind::Value));
                let tag = self.i32_const(ctx, tag as i32);
                let bits = self.slot_bits(ctx, value);
                self.runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, bits])
                    .expect("exception")
            };
            let status = self
                .runtime(ctx, Runtime::ExceptionRaise, vec![exception])
                .expect("status");
            self.define(ctx, op, status);
            return Ok(());
        }
        if Operation::get_op::<ExitOp>(op, ctx).is_some() {
            let code = self.operand(ctx, op, 0)?;
            let status = self
                .runtime(ctx, Runtime::Exit, vec![code])
                .expect("status");
            self.define(ctx, op, status);
            return Ok(());
        }
        if Operation::get_op::<RethrowOp>(op, ctx).is_some() {
            let status = self.operand(ctx, op, 0)?;
            let throw = self.throw_block(ctx);
            self.br(ctx, throw, vec![status]);
            return Ok(());
        }
        if Operation::get_op::<CatchOp>(op, ctx).is_some() {
            let status = self.operand(ctx, op, 0)?;
            let value = self
                .runtime(ctx, Runtime::ExceptionCatch, vec![status])
                .expect("exception");
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<ExceptionMessageOp>(op, ctx).is_some() {
            let exception = self.operand(ctx, op, 0)?;
            let value = self
                .runtime(ctx, Runtime::ExceptionMessage, vec![exception])
                .expect("message");
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(is) = Operation::get_op::<ExceptionIsOp>(op, ctx) {
            let exception = self.operand(ctx, op, 0)?;
            let kind = self.load(ctx, MemKind::I32, exception, perry_runtime::exception::KIND);
            let value = match is.kind(ctx) {
                ErrorKind::Any => {
                    // TypeError, RangeError and Error are 1..=3.
                    let one = self.i32_const(ctx, 1);
                    let offset = self.num(ctx, NumOpcode::I32Sub, vec![kind, one]);
                    let three = self.i32_const(ctx, 3);
                    self.num(ctx, NumOpcode::I32LtU, vec![offset, three])
                }
                other => {
                    let code = self.i32_const(ctx, status_code(other));
                    self.num(ctx, NumOpcode::I32Eq, vec![kind, code])
                }
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<TimerOp>(op, ctx).is_some() {
            return self.lower_timer(ctx, op);
        }
        if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
            // The callee's ramp runs it to its first suspension and returns its promise.
            let callee = call.callee(ctx);
            let args = self.operands(ctx, op)?;
            let i32 = i32_type(ctx);
            let ramp = CallOp::new(ctx, callee, args, vec![i32]).get_operation();
            let promise = self.result(ctx, ramp);
            self.define(ctx, op, promise);
            return Ok(());
        }
        let name = Operation::get_op_dyn(op, ctx).get_opid().to_string();
        bail!("{name} has no lowering to wasm (did canonicalize run?)")
    }
}
