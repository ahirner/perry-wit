//! Perry MIR: the typed middle layer between Perry HIR and Wasm (`TODOs.md`, phase P2).
//!
//! Dialect `perry` is built on [pliron]. Values of [RecordType] are references to
//! closed-shape records (subset rule R1); every allocation carries a memory [Tier]
//! that passes refine from `unassigned` (design decision D3).
//!
//! The control-flow ops are adapted from `pliron-llvm`, which perry-wit must not depend on:
//! that crate's default feature links `llvm-sys`, which the LLVM audit rejects.
mod aggregates;
mod append;
mod attrs;
mod calls;
mod characters;
mod collections;
mod constants;
mod control;
mod diagnostics;
mod effects;
mod exceptions;
mod facts;
mod fast_paths;
mod fold;
mod foreign;
mod globals;
mod heap;
mod integers;
mod intrinsics;
mod json_copy;
mod lifetimes;
mod liveness;
mod locals;
mod optional;
mod pipeline;
mod ranges;
mod regex;
mod scalar;
mod types;
mod unwraps;
mod variants;
mod views;

#[cfg(test)]
mod tests;

pub use attrs::{
    Absence, BinaryKind, CompareKind, ErrorKind, JsonDecoderKind, NumericConstant, Tier,
};
pub use calls::{
    AsyncCallOp, AwaitOp, CallOp, TimerOp, is_async, is_exported, set_async,
    set_atomic_construction, set_exported, set_json_decoder,
};
pub use collections::{
    ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
    ArrayTruncateOp, ConcatOp, ConcatReuseOp, ConcatSegmentsOp, NativeJsonOp, StringConstOp,
    StringLenOp,
};
pub use control::{BrOp, CondBrOp, ReturnOp};
pub use diagnostics::{set_source_span, source_span};
pub use effects::{Allocation, allocates, allocation};
pub use exceptions::{
    CatchOp, CaughtOp, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExitOp, RaiseOp, RethrowOp,
    set_exit_only,
};
pub use facts::{
    ProgramFacts, constrain_signature, specialized_implementation, verify_lossless_module,
    verify_lossless_operations, wit_signature, with_fact_limits,
};
pub use foreign::{ForeignOp, Phase};
pub use globals::{GlobalLoadOp, GlobalOp, INITIALIZER};
pub use heap::{AllocOp, FieldGetOp, FieldPresentOp, FieldSetOp};
pub use integers::BigIntConstOp;
pub use intrinsics::{
    Combinator, Intrinsic, IntrinsicOp, JsonCursorKeyOp, JsonStringifyOp, PromiseCombinatorOp,
    PromiseRejectedOp, PromiseResolvedOp, SettledField, SettledFieldOp, may_throw,
};
pub use liveness::{LiveAfter, LiveValues};
pub use locals::{LoadOp, LocalOp, StoreOp, UndefOp};
pub use optional::{
    IsNullOp, IsUndefinedOp, NullOp, OmittedOp, OptionalCastOp, ToNumberOp, ToStringOp,
    UndefinedOp, UnwrapOp, WrapOp, absence, optional_parts, optional_value,
};
pub use pipeline::{canonicalize, optimization_statistics};
pub use regex::RegexConstOp;
pub use scalar::{BinaryOp, CompareOp, NumericCastOp};
pub use types::{
    AbortType, ArrayType, BigIntType, ByobReaderType, ByteBufferType, ByteInputType, BytesType,
    DateType, DecoderType, ExceptionType, HeadersType, IncomingType, InstantType, JsonCursorType,
    JsonType, NumberStringType, NumericType, OptionalType, PlainDateTimeType, PlainDateType,
    PromiseType, ReadResultType, ReadableType, ReaderType, RecordType, RegexType, RequestType,
    ResourceType, ResponseType, SearchParamsType, SettledType, SlotType, StatsType, StringType,
    TransferType, UrlType, VariantType, WritableType, WriterType,
};
pub use variants::{VariantNewOp, VariantPayloadOp, VariantTagOp, admits_payload, variant_cases};
pub use views::{AggregateViewOp, can_widen};
