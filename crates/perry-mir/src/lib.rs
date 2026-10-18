//! Perry MIR: the typed middle layer between Perry HIR and Wasm (`TODOs.md`, phase P2).
//!
//! Dialect `perry` is built on [pliron]. Values of [RecordType] are references to
//! closed-shape records (subset rule R1); every allocation carries a memory [Tier]
//! that passes refine from `unassigned` (design decision D3).
//!
//! The control-flow ops are adapted from `pliron-llvm`, which perry-wit must not depend on:
//! that crate's default feature links `llvm-sys`, which the LLVM audit rejects.
/// Implements pliron's `SideEffects` for op types whose effect is the same for every
/// instance; `side_effects!(false; …)` lets DCE remove unused ones.
macro_rules! side_effects {
    ($value:expr; $($op:ty),+ $(,)?) => {$(
        #[pliron::derive::op_interface_impl]
        impl pliron::opts::dce::SideEffects for $op {
            fn has_side_effects(&self, _: &pliron::context::Context) -> bool {
                $value
            }
        }
    )+};
}

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
mod json_schema;
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

use pliron::{
    context::{Context, Ptr},
    op::Op,
    operation::Operation,
    r#type::TypeHandle,
    value::Value,
};
use std::{collections::HashSet, hash::Hash};

/// Unlinked operation of `Self` without successors or regions, the shape of every perry op
/// but the branches; pliron's `Operation::new` otherwise repeats both at each constructor.
trait NewOp: Op + Sized {
    fn new_op(ctx: &mut Context, results: Vec<TypeHandle>, operands: Vec<Value>) -> Ptr<Operation> {
        Operation::new(
            ctx,
            Self::get_concrete_op_info(),
            results,
            operands,
            vec![],
            0,
        )
    }
}

impl<T: Op> NewOp for T {}

/// A tuple of op types, so one [is_any] replaces a chain of `get_op` tests.
trait OpSet {
    fn contains(ctx: &Context, op: Ptr<Operation>) -> bool;
}

/// Implements [OpSet] for the tuple of every suffix of the given type parameters.
macro_rules! op_sets {
    ($first:ident $(, $rest:ident)*) => {
        impl<$first: Op $(, $rest: Op)*> OpSet for ($first, $($rest,)*) {
            fn contains(ctx: &Context, op: Ptr<Operation>) -> bool {
                Operation::is_op::<$first>(op, ctx) $(|| Operation::is_op::<$rest>(op, ctx))*
            }
        }
        op_sets!($($rest),*);
    };
    () => {};
}

op_sets!(A, B, C, D, E, F, G, H, I, J, K, L);

/// Whether `op` is an instance of any op type in the tuple `S`.
fn is_any<S: OpSet>(ctx: &Context, op: Ptr<Operation>) -> bool {
    S::contains(ctx, op)
}

/// Every node reachable from `roots` (included) along `successors`, in no particular order.
fn reachable<T, I>(
    roots: impl IntoIterator<Item = T>,
    mut successors: impl FnMut(T) -> I,
) -> HashSet<T>
where
    T: Copy + Eq + Hash,
    I: IntoIterator<Item = T>,
{
    let mut pending: Vec<T> = roots.into_iter().collect();
    let mut seen = HashSet::new();
    while let Some(node) = pending.pop() {
        if seen.insert(node) {
            pending.extend(successors(node));
        }
    }
    seen
}

pub use attrs::{
    Absence, BinaryKind, CompareKind, ErrorKind, FieldOrder, JsonDecoderKind, NumericConstant, Tier,
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
pub use effects::{Allocation, Effects, Failure, Summary, allocates, allocation};
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
pub use json_schema::JsonSchema;
pub use liveness::{LiveAfter, LiveValues};
pub use locals::{LoadOp, LocalOp, StoreOp, UndefOp};
pub use optional::{
    IsNullOp, IsUndefinedOp, NullOp, OmittedOp, OptionalCastOp, ToNumberOp, ToStringOp,
    UndefinedOp, UnwrapOp, WrapOp, absence, admits_undefined, optional_parts, optional_value,
    present_type,
};
pub use pipeline::{canonicalize, optimization_statistics};
pub use regex::RegexConstOp;
pub use scalar::{BinaryOp, CompareOp, NumericCastOp};
pub use types::{
    AbortType, ArrayType, BigIntType, ByobReaderType, ByteBufferType, ByteInputType, BytesType,
    DateType, DecoderType, ExceptionType, HeadersType, IncomingType, InstantType, JsonCursorType,
    JsonType, NumberStringType, NumericType, OptionalType, PlainDateTimeType, PlainDateType,
    PromiseType, ReadResultType, ReadableType, ReaderType, RecordKind, RecordType, RegexType,
    RequestType, ResourceType, ResponseType, SearchParamsType, SettledType, SlotType, StatsType,
    StringType, TransferType, UrlType, VariantType, WritableType, WriterType,
};
pub use variants::{VariantNewOp, VariantPayloadOp, VariantTagOp, admits_payload, variant_cases};
pub use views::{AggregateViewOp, can_widen};
