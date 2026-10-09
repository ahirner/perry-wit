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
//! - Strings are UTF-8 with a cached Unicode scalar length (layout in [perry_runtime::string]);
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
    exceptions::{CaughtStatusOp, MayThrowInterface},
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    intrinsics::{Intrinsic, IntrinsicOp},
    locals::UndefOp,
    optional::{IsUndefinedOp, ToNumberOp, ToStringOp, UndefinedOp, UnwrapOp, WrapOp},
    scalar::{BinaryOp as MirBinaryOp, CompareOp as MirCompareOp, ConvertOp},
    types::{
        ArrayType, BufferType, BytesType, ExceptionType, FsStatsType, JsonType, OptionalType,
        PromiseType, RecordType, StringType, TextDecoderType, TextEncoderType, TextOrBytesType,
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
        types::{FP32Type, FP64Type, FunctionType, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    graph::traversals::region::DFSTraversal,
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
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
mod exceptions;
mod unions;

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
    WaitableSetNew,
    WaitableSetDrop,
    WaitableJoin,
    SubtaskDrop,
    ContextGet0,
    ContextSet0,
    WaitFor,
    TaskCancel,
    ProcessExit,
    /// `task.return` of an async export: (module, function, flat result types).
    TaskReturn(String, String, Vec<boundary::Flat>),
}

impl Host {
    fn symbol(&self) -> Identifier {
        let name = match self {
            Host::WaitableSetNew => "host_waitable_set_new".to_string(),
            Host::WaitableSetDrop => "host_waitable_set_drop".to_string(),
            Host::WaitableJoin => "host_waitable_join".to_string(),
            Host::SubtaskDrop => "host_subtask_drop".to_string(),
            Host::ContextGet0 => "host_context_get_0".to_string(),
            Host::ContextSet0 => "host_context_set_0".to_string(),
            Host::WaitFor => "host_wait_for".to_string(),
            Host::TaskCancel => "host_task_cancel".to_string(),
            Host::ProcessExit => "host_process_exit".to_string(),
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
            Host::TaskCancel => ("[export]$root".into(), "[task-cancel]".into()),
            Host::ProcessExit => ("wasi:cli/exit@0.3.0".into(), "exit-with-code".into()),
            Host::WaitableSetNew => root("[waitable-set-new]"),
            Host::WaitableSetDrop => root("[waitable-set-drop]"),
            Host::WaitableJoin => root("[waitable-join]"),
            Host::SubtaskDrop => root("[subtask-drop]"),
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
            Host::WaitableSetNew | Host::ContextGet0 => (vec![], vec![i32]),
            Host::WaitableSetDrop | Host::SubtaskDrop | Host::ContextSet0 => (vec![i32], vec![]),
            Host::WaitableJoin => (vec![i32, i32], vec![]),
            Host::WaitFor => (vec![i64], vec![i32]),
            Host::TaskCancel => (vec![], vec![]),
            Host::ProcessExit => (vec![i32], vec![]),
            Host::TaskReturn(_, _, flat) => (
                flat.iter().map(|f| boundary::flat_type(ctx, *f)).collect(),
                vec![],
            ),
        }
    }
}

/// Bits of `undefined` in a `number | undefined` slot: a signalling NaN.
pub const UNDEFINED_BITS: u64 = perry_runtime::UNDEFINED_NUMBER;
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
        ErrorKind::SyntaxError => 7,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Runtime {
    TextBytesLength,
    TextBytesTruthy,
    TextBytesEqual,
    FsReadValue,
    FsRead,
    FsWrite,
    FsReadDir,
    FsOptions,
    FsTransferEvent,
    FsTransfersCancel,
    FsStat,
    FsMkdir,
    FsUnlink,
    FsRmdir,
    FsStatsGet,
    FsEvent,
    FsCancel,
    ImportRealloc,
    TextEncoderNew,
    TextEncode,
    TextDecoderNew,
    TextDecode,
    TextDecoderIgnoreBom,
    ExceptionStatus,
    JsonParse,

    JsonStringify,
    JsonNull,
    JsonNumber,
    JsonBoolean,
    JsonString,
    JsonArray,
    JsonObject,
    JsonGet,
    JsonIndex,
    JsonAsNumber,
    JsonAsBoolean,
    JsonAsString,
    JsonAsArray,
    JsonAsObject,
    JsonTypeof,
    JsonEqual,
    JsonIsArray,
    JsonSet,
    JsonSetIndex,

    BufferNew,
    BufferLength,
    BytesNew,
    BytesView,
    BytesFromArray,
    BytesCopy,
    BytesLength,
    BytesOffset,
    BytesBuffer,
    BytesGet,
    BytesPut,
    BytesSubarray,
    BytesSlice,
    BytesSet,
    BytesSetArray,
    BytesAdopt,
    BytesData,
    Alloc,
    ArrayNew,
    ArrayGrow,
    F64Rem,
    TaskEnter,
    TaskResume,
    TaskSuspend,
    TaskFree,
    TaskSet,
    TaskSetSet,
    TaskResult,
    TaskPending,
    TaskFailed,
    TaskSetResult,
    ReadyPop,
    SubtaskRegister,
    SubtaskDone,
    PromiseNew,
    PromiseResolve,
    PromiseReject,
    PromiseWait,
    PromiseRethrow,
    PromiseObserve,
    ExceptionNew,
    ExceptionRaise,
    ExceptionCatch,
    ExceptionIs,
    ExceptionMessage,
    ExceptionEqual,
    StringEqual,
    CabiRealloc,
    StringAdopt,
    StringConcat,
    StringUnit,
    StringCodePoint,
    StringSlice,
    StringSearch,
    StringCase,
    StringTrim,
    StringRepeat,
    StringSplit,
    Stdout,
    Stderr,
    StdoutEvent,
    StderrEvent,
    StdoutCancel,
    StderrCancel,
    SubtasksCancel,
    ExitCodeGet,
    ExitCodeSet,
    ExitCodeClear,
    ExitStatus,
    EnvGet,
    EnvKeys,
    Arguments,
    MonotonicNow,
    DateNow,
}

impl Runtime {
    fn name(self) -> &'static str {
        match self {
            Runtime::ExceptionStatus => "perry_exception_status",
            Runtime::JsonParse => "perry_json_parse",

            Runtime::JsonStringify => "perry_json_stringify",
            Runtime::JsonNull => "perry_json_null",
            Runtime::JsonNumber => "perry_json_number",
            Runtime::JsonBoolean => "perry_json_boolean",
            Runtime::JsonString => "perry_json_string",
            Runtime::JsonArray => "perry_json_array",
            Runtime::JsonObject => "perry_json_object",
            Runtime::JsonGet => "perry_json_get",
            Runtime::JsonIndex => "perry_json_index",
            Runtime::JsonAsNumber => "perry_json_as_number",
            Runtime::JsonAsBoolean => "perry_json_as_boolean",
            Runtime::JsonAsArray => "perry_json_as_array",
            Runtime::JsonAsObject => "perry_json_as_object",
            Runtime::JsonAsString => "perry_json_as_string",
            Runtime::JsonTypeof => "perry_json_typeof",
            Runtime::JsonEqual => "perry_json_equal",
            Runtime::JsonIsArray => "perry_json_is_array",
            Runtime::JsonSet => "perry_json_set",
            Runtime::JsonSetIndex => "perry_json_set_index",
            Runtime::BufferNew => "perry_buffer_new",
            Runtime::BufferLength => "perry_buffer_length",
            Runtime::BytesNew => "perry_bytes_new",
            Runtime::BytesView => "perry_bytes_view",
            Runtime::BytesFromArray => "perry_bytes_from_array",
            Runtime::BytesCopy => "perry_bytes_copy",
            Runtime::BytesLength => "perry_bytes_length",
            Runtime::BytesOffset => "perry_bytes_offset",
            Runtime::BytesBuffer => "perry_bytes_buffer",
            Runtime::BytesGet => "perry_bytes_get",
            Runtime::BytesPut => "perry_bytes_put",
            Runtime::BytesSubarray => "perry_bytes_subarray",
            Runtime::BytesSlice => "perry_bytes_slice",
            Runtime::BytesSet => "perry_bytes_set",
            Runtime::BytesSetArray => "perry_bytes_set_array",
            Runtime::BytesAdopt => "perry_bytes_adopt",
            Runtime::BytesData => "perry_bytes_data",
            Runtime::Alloc => "perry_alloc",
            Runtime::ArrayNew => "perry_array_new",
            Runtime::ArrayGrow => "perry_array_grow",
            Runtime::F64Rem => "perry_f64_rem",
            Runtime::TaskEnter => "perry_task_enter",
            Runtime::TaskResume => "perry_task_resume",
            Runtime::TaskSuspend => "perry_task_suspend",
            Runtime::TaskFree => "perry_task_free",
            Runtime::TaskSet => "perry_task_set",
            Runtime::TaskSetSet => "perry_task_set_set",
            Runtime::TaskResult => "perry_task_result",
            Runtime::TaskPending => "perry_task_pending",
            Runtime::TaskFailed => "perry_task_failed",
            Runtime::TaskSetResult => "perry_task_set_result",
            Runtime::ReadyPop => "perry_ready_pop",
            Runtime::SubtaskRegister => "perry_subtask_register",
            Runtime::SubtaskDone => "perry_subtask_done",
            Runtime::PromiseNew => "perry_promise_new",
            Runtime::PromiseResolve => "perry_promise_resolve",
            Runtime::PromiseReject => "perry_promise_reject",
            Runtime::PromiseWait => "perry_promise_wait",
            Runtime::PromiseRethrow => "perry_promise_rethrow",
            Runtime::PromiseObserve => "perry_promise_observe",
            Runtime::ExceptionNew => "perry_exception_new",
            Runtime::ExceptionRaise => "perry_exception_raise",
            Runtime::ExceptionCatch => "perry_exception_catch",
            Runtime::ExceptionIs => "perry_exception_is",
            Runtime::ExceptionMessage => "perry_exception_message",
            Runtime::ExceptionEqual => "perry_exception_equal",
            Runtime::StringEqual => "perry_string_equal",
            Runtime::CabiRealloc => "perry_cabi_realloc",
            Runtime::FsReadValue => "perry_fs_read_value",
            Runtime::TextBytesLength => "perry_text_bytes_length",
            Runtime::TextBytesTruthy => "perry_text_bytes_truthy",
            Runtime::TextBytesEqual => "perry_text_bytes_equal",
            Runtime::FsRead => "perry_fs_read",
            Runtime::FsWrite => "perry_fs_write",
            Runtime::FsReadDir => "perry_fs_readdir",
            Runtime::FsOptions => "perry_fs_options",
            Runtime::FsTransferEvent => "perry_fs_transfer_event",
            Runtime::FsTransfersCancel => "perry_fs_transfers_cancel",
            Runtime::FsStat => "perry_fs_stat",
            Runtime::FsMkdir => "perry_fs_mkdir",
            Runtime::FsUnlink => "perry_fs_unlink",
            Runtime::FsRmdir => "perry_fs_rmdir",
            Runtime::FsStatsGet => "perry_fs_stats_get",
            Runtime::FsEvent => "perry_fs_event",
            Runtime::FsCancel => "perry_fs_cancel",
            Runtime::ImportRealloc => "perry_import_realloc",
            Runtime::TextEncoderNew => "perry_encoder_new",
            Runtime::TextEncode => "perry_text_encode",
            Runtime::TextDecoderNew => "perry_decoder_new",
            Runtime::TextDecode => "perry_text_decode",
            Runtime::TextDecoderIgnoreBom => "perry_decoder_ignore_bom",
            Runtime::StringAdopt => "perry_string_adopt",
            Runtime::StringConcat => "perry_string_concat",
            Runtime::StringUnit => "perry_string_unit",
            Runtime::StringCodePoint => "perry_string_code_point",
            Runtime::StringSlice => "perry_string_slice",
            Runtime::StringSearch => "perry_string_search",
            Runtime::StringCase => "perry_string_case",
            Runtime::StringTrim => "perry_string_trim",
            Runtime::StringRepeat => "perry_string_repeat",
            Runtime::StringSplit => "perry_string_split",

            Runtime::Stdout => "perry_stdout",
            Runtime::Stderr => "perry_stderr",
            Runtime::StdoutCancel => "perry_stdout_cancel",
            Runtime::StderrCancel => "perry_stderr_cancel",
            Runtime::SubtasksCancel => "perry_subtasks_cancel",
            Runtime::ExitCodeGet => "perry_process_get_exit_code",
            Runtime::ExitCodeSet => "perry_process_set_exit_code",
            Runtime::ExitCodeClear => "perry_process_clear_exit_code",
            Runtime::ExitStatus => "perry_process_exit_status",
            Runtime::StdoutEvent => "perry_stdout_event",
            Runtime::StderrEvent => "perry_stderr_event",
            Runtime::EnvGet => "perry_env_get",
            Runtime::EnvKeys => "perry_env_keys",
            Runtime::Arguments => "perry_arguments",
            Runtime::MonotonicNow => "perry_monotonic_now",
            Runtime::DateNow => "perry_date_now",
        }
    }

    fn symbol(self) -> Identifier {
        format!("rt_{}", &self.name()["perry_".len()..])
            .try_into()
            .expect("identifier")
    }

    /// (parameters, results) as (f64?, count) pairs: all runtime values are i32 or f64.
    fn signature(self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let i32 = i32_type(ctx);
        let i64: TypeHandle = IntegerType::get(ctx, 64, Signedness::Signless).into();
        let f64: TypeHandle = FP64Type::get(ctx).into();
        match self {
            Runtime::ExceptionStatus => (vec![], vec![i32]),
            Runtime::JsonNull => (vec![], vec![i32]),
            Runtime::JsonNumber => (vec![f64], vec![i32]),
            Runtime::JsonBoolean
            | Runtime::JsonString
            | Runtime::JsonArray
            | Runtime::JsonParse
            | Runtime::JsonStringify
            | Runtime::JsonAsArray
            | Runtime::JsonAsObject
            | Runtime::JsonAsString
            | Runtime::JsonAsBoolean
            | Runtime::JsonTypeof
            | Runtime::JsonIsArray => (vec![i32], vec![i32]),
            Runtime::JsonObject | Runtime::JsonGet | Runtime::JsonEqual => {
                (vec![i32, i32], vec![i32])
            }
            Runtime::JsonIndex => (vec![i32, f64], vec![i32]),
            Runtime::JsonAsNumber => (vec![i32], vec![f64]),
            Runtime::JsonSet => (vec![i32, i32, i32], vec![]),
            Runtime::JsonSetIndex => (vec![i32, f64, i32], vec![]),
            Runtime::BufferNew | Runtime::BytesNew => (vec![f64], vec![i32]),
            Runtime::BufferLength | Runtime::BytesLength | Runtime::BytesOffset => {
                (vec![i32], vec![f64])
            }
            Runtime::BytesFromArray | Runtime::BytesCopy | Runtime::BytesBuffer => {
                (vec![i32], vec![i32])
            }
            Runtime::BytesGet => (vec![i32, f64], vec![f64]),
            Runtime::BytesPut => (vec![i32, f64, f64], vec![f64]),
            Runtime::BytesView | Runtime::BytesSubarray | Runtime::BytesSlice => {
                (vec![i32, f64, f64], vec![i32])
            }
            Runtime::BytesSet | Runtime::BytesSetArray => (vec![i32, i32, f64], vec![i32]),
            Runtime::BytesAdopt => (vec![i32, i32], vec![i32]),
            Runtime::BytesData => (vec![i32], vec![i32]),
            Runtime::ExitCodeGet => (vec![], vec![f64]),
            Runtime::ExitStatus => (vec![], vec![i32]),
            Runtime::ExitCodeSet => (vec![f64], vec![i32]),
            Runtime::ExitCodeClear
            | Runtime::SubtasksCancel
            | Runtime::StdoutCancel
            | Runtime::StderrCancel => (vec![], vec![]),
            Runtime::Stdout | Runtime::Stderr => (vec![i32], vec![]),
            Runtime::StdoutEvent | Runtime::StderrEvent => (vec![i32; 3], vec![]),
            Runtime::EnvGet => (vec![i32], vec![i32]),
            Runtime::EnvKeys | Runtime::Arguments => (vec![], vec![i32]),
            Runtime::MonotonicNow | Runtime::DateNow => (vec![], vec![f64]),
            Runtime::ExceptionNew => (vec![i32, i64], vec![i32]),
            Runtime::ExceptionRaise
            | Runtime::ExceptionCatch
            | Runtime::ExceptionMessage
            | Runtime::PromiseRethrow => (vec![i32], vec![i32]),
            Runtime::ExceptionIs | Runtime::ExceptionEqual | Runtime::StringEqual => {
                (vec![i32, i32], vec![i32])
            }
            Runtime::Alloc | Runtime::ArrayNew => (vec![i32], vec![i32]),
            Runtime::ArrayGrow => (vec![i32], vec![]),
            Runtime::F64Rem => (vec![f64, f64], vec![f64]),
            Runtime::TaskFree => (vec![], vec![]),
            Runtime::TaskEnter
            | Runtime::TaskSuspend
            | Runtime::TaskSet
            | Runtime::TaskResult
            | Runtime::TaskPending
            | Runtime::TaskFailed
            | Runtime::ReadyPop
            | Runtime::PromiseNew => (vec![], vec![i32]),
            Runtime::TaskResume
            | Runtime::TaskSetSet
            | Runtime::TaskSetResult
            | Runtime::PromiseObserve => (vec![i32], vec![]),
            Runtime::SubtaskDone => (vec![i32], vec![i32]),
            Runtime::SubtaskRegister => (vec![i32, i32, i64], vec![]),
            Runtime::PromiseResolve => (vec![i32, i64], vec![]),
            Runtime::PromiseReject => (vec![i32, i32], vec![]),
            Runtime::PromiseWait => (vec![i32, i32], vec![i32]),
            Runtime::CabiRealloc => (vec![i32; 4], vec![i32]),
            Runtime::StringUnit => (vec![i32, f64, i32], vec![i32]),
            Runtime::StringCodePoint => (vec![i32, f64], vec![f64]),
            Runtime::StringSlice => (vec![i32, f64, f64, i32], vec![i32]),
            Runtime::StringSearch => (vec![i32, i32, f64, i32], vec![f64]),
            Runtime::StringCase | Runtime::StringTrim => (vec![i32, i32], vec![i32]),
            Runtime::StringRepeat => (vec![i32, f64], vec![i32]),
            Runtime::StringSplit => (vec![i32, i32, f64], vec![i32]),
            Runtime::FsStat | Runtime::FsMkdir | Runtime::FsUnlink | Runtime::FsRmdir => {
                (vec![i32, i32], vec![i32])
            }
            Runtime::FsRead | Runtime::FsReadValue => (vec![i32; 3], vec![i32]),
            Runtime::TextBytesLength => (vec![i64], vec![f64]),
            Runtime::TextBytesTruthy => (vec![i64], vec![i32]),
            Runtime::TextBytesEqual => (vec![i64, i64], vec![i32]),
            Runtime::FsWrite => (vec![i32; 4], vec![i32]),
            Runtime::FsReadDir => (vec![i32; 2], vec![i32]),
            Runtime::FsOptions => (vec![i32, i32, f64, i32], vec![i32]),
            Runtime::FsStatsGet => (vec![i32, i32], vec![f64]),
            Runtime::FsEvent | Runtime::FsTransferEvent => (vec![i32; 3], vec![]),
            Runtime::FsCancel | Runtime::FsTransfersCancel => (vec![], vec![]),
            Runtime::ImportRealloc => (vec![i32; 4], vec![i32]),
            Runtime::TextEncoderNew => (vec![], vec![i32]),
            Runtime::TextEncode | Runtime::TextDecoderIgnoreBom => (vec![i32], vec![i32]),
            Runtime::TextDecoderNew | Runtime::TextDecode => (vec![i32, i32, i32], vec![i32]),
            Runtime::StringAdopt | Runtime::StringConcat => (vec![i32, i32], vec![i32]),
        }
    }
}

fn i32_type(ctx: &mut Context) -> TypeHandle {
    IntegerType::get(ctx, 32, Signedness::Signless).into()
}

/// How a MIR type is represented in Wasm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Repr {
    I64,
    F64,
    Bool,
    I32,
    Ref,
}

fn repr(ctx: &Context, ty: TypeHandle) -> Result<Repr> {
    let deref = ty.deref(ctx);
    if deref.is::<TextOrBytesType>() {
        return Ok(Repr::I64);
    }
    if deref.is::<FP64Type>() {
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
        || deref.is::<ExceptionType>()
        || deref.is::<JsonType>()
        || deref.is::<FsStatsType>()
        || deref.is::<TextEncoderType>()
        || deref.is::<TextDecoderType>()
        || deref.is::<BytesType>()
        || deref.is::<BufferType>()
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
        Repr::I64 => IntegerType::get(ctx, 64, Signedness::Signless).into(),
        _ => i32_type(ctx),
    })
}

fn slot_kind(ctx: &Context, ty: TypeHandle) -> Result<MemKind> {
    Ok(match repr(ctx, ty)? {
        Repr::F64 => MemKind::F64,
        Repr::I64 => MemKind::I64,
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
            } else if op_cast::<dyn MayThrowInterface>(Operation::get_op_dyn(op, ctx).as_ref())
                .is_some_and(|op| op.may_throw(ctx))
            {
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
        let scalars = text.chars().count() as u32;
        for word in [
            text.len() as u32,
            scalars,
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
    uses_process: bool,
    needs_task_cleanup: bool,
    uses_filesystem: bool,
}

impl ModuleState {
    fn uses_filesystem_transfers(&self) -> bool {
        [
            Runtime::FsRead,
            Runtime::FsReadValue,
            Runtime::FsWrite,
            Runtime::FsReadDir,
        ]
        .iter()
        .any(|entry| self.runtime.contains(entry))
    }

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
        uses_process: false,
        needs_task_cleanup: false,
        uses_filesystem: false,
    };
    let mut raw_sync_exports = Vec::new();
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
            raw_sync_exports.push((symbol, lowered));
        }
    }
    for (symbol, function) in raw_sync_exports {
        let wrapper = export_wrapper(ctx, &mut state, &function, &symbol)?;
        state.wasm.append_operation(ctx, wrapper.get_operation(), 0);
    }
    if matches!(exports, Exports::Raw) {
        ensure!(
            !state.uses_filesystem,
            "filesystem operations require async WIT exports"
        );
        ensure!(
            !state.runtime.contains(&Runtime::Stdout) && !state.runtime.contains(&Runtime::Stderr),
            "console output requires async WIT exports to drain P3 streams"
        );
    }
    if !async_functions.is_empty() {
        async_split::drive(ctx, &mut state, &async_functions);
    }
    if let Exports::Canonical { exports, .. } = exports {
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
    boundary::task_cleanup(ctx, &mut state);
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
    let mut b = FunctionLowering::glue(state, ctx, &wrapper);
    b.runtime(ctx, Runtime::TaskEnter, vec![]);
    let call = CallOp::new(ctx, name.clone(), args, results).get_operation();
    b.insert(ctx, call);
    let values: Vec<_> = call.deref(ctx).results().collect();
    if b.state.throws.contains(name) {
        let failed = b.new_block(ctx, vec![]);
        let normal = b.new_block(ctx, vec![]);
        b.cond_br(
            ctx,
            *values.last().unwrap(),
            (failed, vec![]),
            (normal, vec![]),
        );
        b.block = failed;
        b.abort_task(ctx);
        let ret = ReturnOp::new(ctx, values.clone()).get_operation();
        b.insert(ctx, ret);
        b.block = normal;
    }
    b.finish_sync(ctx);
    b.runtime(ctx, Runtime::TaskFree, vec![]);
    let ret = ReturnOp::new(ctx, values).get_operation();
    b.insert(ctx, ret);
    for param in params {
        ensure!(
            !matches!(repr(ctx, param)?, Repr::Ref | Repr::I64),
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
    let zero = b.zero(ctx, value_wasm);
    let value = b.select(ctx, fulfilled, value, zero);
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
        let region = mir.get_region(ctx);
        let mir_blocks: Vec<_> = DFSTraversal::new(ctx, &region)
            .reverse_post_order()
            .collect();
        for mir_block in mir_blocks {
            self.block = self.blocks[&mir_block];
            let args: Vec<_> = mir_block.deref(ctx).arguments().collect();
            self.spill(ctx, &args);
            let ops: Vec<_> = mir_block.deref(ctx).iter(ctx).collect();
            let mut ops = ops.into_iter().peekable();
            while let Some(op) = ops.next() {
                let caught = ops
                    .peek()
                    .copied()
                    .filter(|next| Operation::get_op::<CaughtStatusOp>(*next, ctx).is_some());
                let results: Vec<_> = op.deref(ctx).results().collect();
                if caught.is_some() {
                    let types = results
                        .iter()
                        .map(|v| wasm_type(ctx, v.get_type(ctx)))
                        .collect::<Result<Vec<_>>>()?;
                    let mut args = vec![i32_type(ctx)];
                    args.extend(&types);
                    let landing = self.new_block(ctx, args);
                    self.landing = Some((landing, types));
                }
                self.op(ctx, op).with_context(|| {
                    format!("lowering {}", Operation::get_op_dyn(op, ctx).get_opid())
                })?;
                if let Some(caught) = caught {
                    ops.next();
                    let (landing, _) = self.landing.take().expect("caught operation");
                    let mut args = vec![self.i32_const(ctx, 0)];
                    args.extend(results.iter().map(|v| self.values[v]));
                    self.br(ctx, landing, args);
                    self.block = landing;
                    let args: Vec<_> = landing.deref(ctx).arguments().collect();
                    self.define(ctx, caught, args[0]);
                    self.values
                        .extend(results.iter().copied().zip(args.into_iter().skip(1)));
                    let status = caught.deref(ctx).get_result(0);
                    self.spill(ctx, &[status]);
                }
                self.spill(ctx, &results);
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
            let zero = self.zero(ctx, ty);
            values.push(zero);
        }
        values.push(block.deref(ctx).get_argument(0));
        let op = ReturnOp::new(ctx, values).get_operation();
        self.insert(ctx, op);
        self.block = saved;
        self.throw_block = Some(block);
        block
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
        let throw = self.exception_target(ctx, status);
        let next = self.new_block(ctx, vec![]);
        self.cond_br(ctx, failed, throw, (next, vec![]));
        self.block = next;
    }

    // --- optional values ----------------------------------------------------------------------

    /// Promote a present value without letting a host NaN become the absent-number sentinel.
    fn wrap_defined(&mut self, ctx: &mut Context, value: Value, inner: TypeHandle) -> Value {
        if inner.deref(ctx).is::<FP64Type>() {
            let is_nan = self.num(ctx, NumOpcode::F64Ne, vec![value, value]);
            let nan = self.f64_const(ctx, f64::NAN);
            self.select(ctx, is_nan, nan, value)
        } else {
            value
        }
    }

    fn zero(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        if ty.deref(ctx).is::<FP64Type>() {
            self.f64_const(ctx, 0.0)
        } else if ty.deref(ctx).is::<FP32Type>() {
            let zero = self.f64_const(ctx, 0.0);
            self.num(ctx, NumOpcode::F32DemoteF64, vec![zero])
        } else if ty
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() == 64)
        {
            self.i64_const(ctx, 0)
        } else {
            self.i32_const(ctx, 0)
        }
    }

    fn undefined(&mut self, ctx: &mut Context, inner: TypeHandle) -> Result<Value> {
        Ok(match repr(ctx, inner)? {
            Repr::F64 => {
                let bits = self.i64_const(ctx, UNDEFINED_BITS);
                self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits])
            }
            Repr::Bool => self.i32_const(ctx, UNDEFINED_BOOL),
            Repr::Ref => self.i32_const(ctx, 0),
            Repr::I64 => self.i64_const(ctx, 0),
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
            Repr::I64 => self.num(ctx, NumOpcode::I64Eqz, vec![value]),
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

    // --- ops ----------------------------------------------------------------------------------

    fn op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        if self.lower_exception_op(ctx, op)? || self.lower_union_op(ctx, op)? {
            return Ok(());
        }
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
                Repr::I64 => self.i64_const(ctx, 0),
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
            let inner = optional_inner(ctx, mir_ty).unwrap_or(mir_ty);
            let value =
                if inner.deref(ctx).is::<StringType>() || inner.deref(ctx).is::<ExceptionType>() {
                    let function = if inner.deref(ctx).is::<StringType>() {
                        Runtime::StringEqual
                    } else {
                        Runtime::ExceptionEqual
                    };
                    let eq = self.runtime(ctx, function, operands).expect("equality");
                    match kind {
                        CompareKind::Eq => eq,
                        CompareKind::Ne => self.num(ctx, NumOpcode::I32Eqz, vec![eq]),
                        _ => bail!("ordered comparison of strings or exceptions is not supported"),
                    }
                } else if let Some(inner) = optional_inner(ctx, mir_ty)
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
                self.throw_status_if(ctx, status, status);
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
            let loaded = self.wrap_defined(ctx, loaded, element);
            let undefined = self.undefined(ctx, optional_inner(ctx, element).unwrap_or(element))?;
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
            let inner = op.deref(ctx).get_operand(0).get_type(ctx);
            let value = self.wrap_defined(ctx, value, inner);
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
            let value = self.load(ctx, MemKind::I32, string, perry_runtime::string::SCALARS);
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
        if let Some(intrinsic) = Operation::get_op::<IntrinsicOp>(op, ctx) {
            let kind = intrinsic.kind(ctx);
            let numeric = match kind {
                Intrinsic::MathFloor => Some(NumOpcode::F64Floor),
                Intrinsic::MathCeil => Some(NumOpcode::F64Ceil),
                Intrinsic::MathTrunc => Some(NumOpcode::F64Trunc),
                Intrinsic::MathAbs => Some(NumOpcode::F64Abs),
                Intrinsic::MathSqrt => Some(NumOpcode::F64Sqrt),
                _ => None,
            };
            if let Some(opcode) = numeric {
                let args = self.operands(ctx, op)?;
                let result = self.num(ctx, opcode, args);
                self.define(ctx, op, result);
                return Ok(());
            }
            if matches!(kind, Intrinsic::MathRound | Intrinsic::DateClip) {
                let value = self.operands(ctx, op)?[0];
                let result = if kind == Intrinsic::MathRound {
                    let floor = self.num(ctx, NumOpcode::F64Floor, vec![value]);
                    let fraction = self.num(ctx, NumOpcode::F64Sub, vec![value, floor]);
                    let half = self.f64_const(ctx, 0.5);
                    let up = self.num(ctx, NumOpcode::F64Ge, vec![fraction, half]);
                    let one = self.f64_const(ctx, 1.0);
                    let ceil = self.num(ctx, NumOpcode::F64Add, vec![floor, one]);
                    let rounded = self.select(ctx, up, ceil, floor);
                    self.num(ctx, NumOpcode::F64Copysign, vec![rounded, value])
                } else {
                    let magnitude = self.num(ctx, NumOpcode::F64Abs, vec![value]);
                    let limit = self.f64_const(ctx, 8_640_000_000_000_000.0);
                    let valid = self.num(ctx, NumOpcode::F64Le, vec![magnitude, limit]);
                    let truncated = self.num(ctx, NumOpcode::F64Trunc, vec![value]);
                    let zero = self.f64_const(ctx, 0.0);
                    let clipped = self.num(ctx, NumOpcode::F64Add, vec![truncated, zero]);
                    let nan = self.f64_const(ctx, f64::NAN);
                    self.select(ctx, valid, clipped, nan)
                };
                self.define(ctx, op, result);
                return Ok(());
            }
            let value_runtime = match kind {
                Intrinsic::FsOptions => Some((Runtime::FsOptions, None)),
                Intrinsic::TextBytesLength => Some((Runtime::TextBytesLength, None)),
                Intrinsic::TextBytesTruthy => Some((Runtime::TextBytesTruthy, None)),
                Intrinsic::TextBytesEqual => Some((Runtime::TextBytesEqual, None)),
                Intrinsic::FsSize => Some((Runtime::FsStatsGet, Some(0))),
                Intrinsic::FsMtime => Some((Runtime::FsStatsGet, Some(1))),
                Intrinsic::FsKind => Some((Runtime::FsStatsGet, Some(2))),
                Intrinsic::TextEncoderNew => Some((Runtime::TextEncoderNew, None)),
                Intrinsic::TextEncode => Some((Runtime::TextEncode, None)),
                Intrinsic::TextDecoderNew => Some((Runtime::TextDecoderNew, None)),
                Intrinsic::TextDecode => Some((Runtime::TextDecode, None)),
                Intrinsic::TextDecoderIgnoreBom => Some((Runtime::TextDecoderIgnoreBom, None)),
                Intrinsic::StringCharAt => Some((Runtime::StringUnit, Some(0))),
                Intrinsic::StringAt => Some((Runtime::StringUnit, Some(1))),
                Intrinsic::StringIndex => Some((Runtime::StringUnit, Some(2))),
                Intrinsic::StringCodePointAt => Some((Runtime::StringCodePoint, None)),
                Intrinsic::StringSlice => Some((Runtime::StringSlice, Some(0))),
                Intrinsic::StringSubstring => Some((Runtime::StringSlice, Some(1))),
                Intrinsic::StringIndexOf => Some((Runtime::StringSearch, Some(0))),
                Intrinsic::StringLastIndexOf => Some((Runtime::StringSearch, Some(1))),
                Intrinsic::StringStartsWith => Some((Runtime::StringSearch, Some(2))),
                Intrinsic::StringEndsWith => Some((Runtime::StringSearch, Some(3))),
                Intrinsic::StringLower => Some((Runtime::StringCase, Some(0))),
                Intrinsic::StringUpper => Some((Runtime::StringCase, Some(1))),
                Intrinsic::StringTrim => Some((Runtime::StringTrim, Some(0))),
                Intrinsic::StringTrimStart => Some((Runtime::StringTrim, Some(1))),
                Intrinsic::StringTrimEnd => Some((Runtime::StringTrim, Some(2))),
                Intrinsic::StringRepeat => Some((Runtime::StringRepeat, None)),
                Intrinsic::StringSplit => Some((Runtime::StringSplit, None)),
                _ => None,
            };
            if let Some((runtime, mode)) = value_runtime {
                let mut operands = self.operands(ctx, op)?;
                if let Some(mode) = mode {
                    operands.push(self.i32_const(ctx, mode));
                }
                let result = self
                    .runtime(ctx, runtime, operands)
                    .expect("string operation result");
                if intrinsic.may_throw(ctx) {
                    self.check_pending(ctx);
                }
                self.define(ctx, op, result);
                return Ok(());
            }
            let json_runtime = match kind {
                Intrinsic::JsonParse => Some(Runtime::JsonParse),
                Intrinsic::JsonStringify => Some(Runtime::JsonStringify),
                Intrinsic::JsonNull => Some(Runtime::JsonNull),
                Intrinsic::JsonNumber => Some(Runtime::JsonNumber),
                Intrinsic::JsonBoolean => Some(Runtime::JsonBoolean),
                Intrinsic::JsonString => Some(Runtime::JsonString),
                Intrinsic::JsonArray => Some(Runtime::JsonArray),
                Intrinsic::JsonObject => Some(Runtime::JsonObject),
                Intrinsic::JsonGet => Some(Runtime::JsonGet),
                Intrinsic::JsonIndex => Some(Runtime::JsonIndex),
                Intrinsic::JsonAsNumber => Some(Runtime::JsonAsNumber),
                Intrinsic::JsonAsBoolean => Some(Runtime::JsonAsBoolean),
                Intrinsic::JsonAsArray => Some(Runtime::JsonAsArray),
                Intrinsic::JsonAsObject => Some(Runtime::JsonAsObject),
                Intrinsic::JsonAsString => Some(Runtime::JsonAsString),
                Intrinsic::JsonTypeof => Some(Runtime::JsonTypeof),
                Intrinsic::JsonEqual => Some(Runtime::JsonEqual),
                Intrinsic::JsonIsArray => Some(Runtime::JsonIsArray),
                Intrinsic::JsonSet => Some(Runtime::JsonSet),
                Intrinsic::JsonSetIndex => Some(Runtime::JsonSetIndex),
                _ => None,
            };
            if let Some(runtime) = json_runtime {
                let operands = self.operands(ctx, op)?;
                let result = self.runtime(ctx, runtime, operands);
                if intrinsic.may_throw(ctx) {
                    self.check_pending(ctx);
                }
                if let Some(result) = result {
                    self.define(ctx, op, result);
                }
                return Ok(());
            }
            let bytes_runtime = match kind {
                Intrinsic::BufferNew => Some(Runtime::BufferNew),
                Intrinsic::BufferLength => Some(Runtime::BufferLength),
                Intrinsic::BytesNew => Some(Runtime::BytesNew),
                Intrinsic::BytesView => Some(Runtime::BytesView),
                Intrinsic::BytesFromArray => Some(Runtime::BytesFromArray),
                Intrinsic::BytesCopy => Some(Runtime::BytesCopy),
                Intrinsic::BytesLength => Some(Runtime::BytesLength),
                Intrinsic::BytesOffset => Some(Runtime::BytesOffset),
                Intrinsic::BytesBuffer => Some(Runtime::BytesBuffer),
                Intrinsic::BytesGet => Some(Runtime::BytesGet),
                Intrinsic::BytesPut => Some(Runtime::BytesPut),
                Intrinsic::BytesSubarray => Some(Runtime::BytesSubarray),
                Intrinsic::BytesSlice => Some(Runtime::BytesSlice),
                Intrinsic::BytesSet => Some(Runtime::BytesSet),
                Intrinsic::BytesSetArray => Some(Runtime::BytesSetArray),
                _ => None,
            };
            if let Some(runtime) = bytes_runtime {
                let operands = self.operands(ctx, op)?;
                let result = self
                    .runtime(ctx, runtime, operands)
                    .expect("byte operation result");
                match kind {
                    Intrinsic::BufferNew | Intrinsic::BytesNew | Intrinsic::BytesView => {
                        let failed = self.num(ctx, NumOpcode::I32Eqz, vec![result]);
                        let status =
                            self.i32_const(ctx, perry_runtime::exception::RANGE_ERROR as i32);
                        self.throw_status_if(ctx, failed, status);
                        self.define(ctx, op, result);
                    }
                    Intrinsic::BytesSet | Intrinsic::BytesSetArray => {
                        self.throw_status_if(ctx, result, result)
                    }
                    _ => self.define(ctx, op, result),
                }
                return Ok(());
            }
            if matches!(
                kind,
                Intrinsic::ExitCodeGet
                    | Intrinsic::ExitCodeSet
                    | Intrinsic::ExitCodeClear
                    | Intrinsic::ProcessExit
                    | Intrinsic::ProcessExitCurrent
            ) {
                ensure!(
                    self.state.can_import("wasi:cli/exit@0.3.0"),
                    "WIT world must import wasi:cli/exit@0.3.0"
                );
                self.state.uses_process = true;
                match kind {
                    Intrinsic::ExitCodeGet => {
                        let value = self
                            .runtime(ctx, Runtime::ExitCodeGet, vec![])
                            .expect("exit code");
                        self.define(ctx, op, value);
                    }
                    Intrinsic::ExitCodeClear => {
                        self.runtime(ctx, Runtime::ExitCodeClear, vec![]);
                    }
                    Intrinsic::ExitCodeSet | Intrinsic::ProcessExit => {
                        let value = self.operand(ctx, op, 0)?;
                        let status = self
                            .runtime(ctx, Runtime::ExitCodeSet, vec![value])
                            .expect("status");
                        self.throw_status_if(ctx, status, status);
                        if kind == Intrinsic::ExitCodeSet {
                            self.define(ctx, op, value);
                        } else {
                            self.exit_process(ctx);
                        }
                    }
                    Intrinsic::ProcessExitCurrent => self.exit_process(ctx),
                    _ => unreachable!(),
                }
                return Ok(());
            }
            if matches!(
                kind,
                Intrinsic::FsStat
                    | Intrinsic::FsMkdir
                    | Intrinsic::FsUnlink
                    | Intrinsic::FsRmdir
                    | Intrinsic::FsReadText
                    | Intrinsic::FsReadBytes
                    | Intrinsic::FsReadValue
                    | Intrinsic::FsWriteValue
                    | Intrinsic::FsWriteText
                    | Intrinsic::FsWriteBytes
                    | Intrinsic::FsReadDir
            ) {
                self.state.uses_filesystem = true;
                ensure!(
                    self.state.can_import("wasi:filesystem/preopens@0.3.0"),
                    "WIT world must import wasi:filesystem/preopens@0.3.0"
                );
            }
            let (runtime, interface) = match intrinsic.kind(ctx) {
                Intrinsic::FsReadValue => (Runtime::FsReadValue, "wasi:filesystem/types@0.3.0"),
                Intrinsic::FsReadText | Intrinsic::FsReadBytes => {
                    (Runtime::FsRead, "wasi:filesystem/types@0.3.0")
                }
                Intrinsic::FsWriteText | Intrinsic::FsWriteBytes | Intrinsic::FsWriteValue => {
                    (Runtime::FsWrite, "wasi:filesystem/types@0.3.0")
                }
                Intrinsic::FsReadDir => (Runtime::FsReadDir, "wasi:filesystem/types@0.3.0"),
                Intrinsic::FsStat => (Runtime::FsStat, "wasi:filesystem/types@0.3.0"),
                Intrinsic::FsMkdir => (Runtime::FsMkdir, "wasi:filesystem/types@0.3.0"),
                Intrinsic::FsUnlink => (Runtime::FsUnlink, "wasi:filesystem/types@0.3.0"),
                Intrinsic::FsRmdir => (Runtime::FsRmdir, "wasi:filesystem/types@0.3.0"),
                Intrinsic::Stdout => (Runtime::Stdout, "wasi:cli/stdout@0.3.0"),
                Intrinsic::Stderr => (Runtime::Stderr, "wasi:cli/stderr@0.3.0"),
                Intrinsic::EnvGet => (Runtime::EnvGet, "wasi:cli/environment@0.3.0"),
                Intrinsic::EnvKeys => (Runtime::EnvKeys, "wasi:cli/environment@0.3.0"),
                Intrinsic::Arguments => (Runtime::Arguments, "wasi:cli/environment@0.3.0"),
                Intrinsic::MonotonicNow => (Runtime::MonotonicNow, MONOTONIC_CLOCK),
                Intrinsic::DateNow => (Runtime::DateNow, "wasi:clocks/system-clock@0.3.0"),
                _ => unreachable!("runtime intrinsic handled above"),
            };
            ensure!(
                self.state.can_import(interface),
                "WIT world must import {interface}"
            );
            let count = op.deref(ctx).get_num_operands();
            let mut operands = (0..count)
                .map(|i| self.operand(ctx, op, i))
                .collect::<Result<Vec<_>>>()?;
            if matches!(
                kind,
                Intrinsic::FsReadText
                    | Intrinsic::FsReadBytes
                    | Intrinsic::FsWriteText
                    | Intrinsic::FsWriteBytes
            ) {
                let text = self.i32_const(
                    ctx,
                    i32::from(matches!(
                        kind,
                        Intrinsic::FsReadText | Intrinsic::FsWriteText
                    )),
                );
                operands.insert(operands.len() - 1, text);
            }
            if kind == Intrinsic::FsWriteValue {
                let value = operands[1];
                operands[1] = self.num(ctx, NumOpcode::I32WrapI64, vec![value]);
                let text = self.text_bytes_is_text(ctx, value);
                operands.insert(2, text);
            }
            if let Some(value) = self.runtime(ctx, runtime, operands) {
                self.define(ctx, op, value);
            }
            return Ok(());
        }
        if Operation::get_op::<AwaitOp>(op, ctx).is_some() {
            return self.lower_await(ctx, op);
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
