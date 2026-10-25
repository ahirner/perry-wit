//! Integer range analysis for eliding bounds checks on proven in-bounds accesses.
//!
//! The shared numeric analysis supplies non-negative integer ranges, including widened
//! loop-carried counts. This pass retains object bounds and buffer-detachment legality.
//!
//! A conditional branch on `i < length(a)` (or `length(a) > i`, or the negations on the false
//! edge) with a non-negative integer `i` proves `0 <= i < length(a)` in the blocks dominated
//! by that edge's target when the target has a single predecessor. On every path to such a
//! block, the last execution of the target follows the last definition of `i`, so the proof
//! holds for the current value of `i`. Within those blocks, `a[i]` rewrites to an unchecked
//! element access under these conditions:
//!
//! - Arrays retain a proven index when every operation between the length read and the element
//!   access cannot shrink the array, using converged mutation and object-identity facts for calls.
//! - A `Uint8Array` shrinks to length 0 only when its buffer is transferred during a BYOB read,
//!   which requires a call or an `await`. The proof holds when the path between the length read
//!   and the element read is free of calls, awaits, and BYOB reads.
//!
//! Without a length comparison, an index is in bounds when its integral range lies below the
//! array's minimum length, or within the constructor or subview extent of a view that stays
//! attached. Dominating comparisons of integral operands narrow those ranges, also through
//! `+`, `-`, `*` and numeric casts.
//!
//! The unchecked read's result is wrapped back into `T | undefined`, and `fold` subsequently
//! eliminates redundant `undefined` tests and coercions on the wrapped value.
//! Proven stores cannot append or throw and need no allocation safe point.
//!
//! Strings are immutable, so `s.charAt(i)` under a proof of `i < s.length` rewrites to
//! `string_char_at_index`.
use crate::{
    aggregates::functions,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    collections::{ArrayAtOp, ArrayGetOp, ArrayLenOp, ArraySetOp, ArrayStoreOp, StringLenOp},
    facts::guards::{GuardedRanges, orders},
    intrinsics::{Intrinsic, IntrinsicOp, intrinsic_of as intrinsic},
    optional::WrapOp,
    scalar::NumericCastOp,
    types::ArrayType,
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::IntegerAttr,
        op_interfaces::{OneRegionInterface, OneResultInterface},
        ops::{ConstantOp, FuncOp},
        types::{FP64Type, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    graph::dominance::DomInfo,
    irbuild::IRStatus,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{Typed, TypedHandle},
    utils::apint::{APInt, bw},
    value::Value,
};
use std::collections::HashMap;

mod bytes;

/// What a length is the length of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Length {
    Array(Value, Ptr<Operation>),
    /// A view, and the operation that read its length.
    Bytes(Value, Ptr<Operation>),
    /// A string's scalar count.
    String(Value),
}

/// Facts and indexed accesses match by kind and object.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Object {
    Array,
    Bytes,
    String,
}

impl Length {
    fn object(self) -> Value {
        match self {
            Length::Array(value, _) | Length::Bytes(value, _) | Length::String(value) => value,
        }
    }

    fn kind(self) -> Object {
        match self {
            Length::Array(..) => Object::Array,
            Length::Bytes(..) => Object::Bytes,
            Length::String(_) => Object::String,
        }
    }
}

/// Recognize array/string lengths converted to numbers and byte-view lengths.
fn length(ctx: &Context, value: Value) -> Option<Length> {
    let op = value.defining_op()?;
    if Operation::get_op::<NumericCastOp>(op, ctx).is_some()
        && value.get_type(ctx).deref(ctx).is::<FP64Type>()
    {
        let len = op.deref(ctx).get_operand(0).defining_op()?;
        if Operation::get_op::<StringLenOp>(len, ctx).is_some() {
            return Some(Length::String(len.deref(ctx).get_operand(0)));
        }
        Operation::get_op::<ArrayLenOp>(len, ctx)?;
        return Some(Length::Array(len.deref(ctx).get_operand(0), len));
    }
    (intrinsic(ctx, op) == Some(Intrinsic::BytesLength))
        .then(|| Length::Bytes(op.deref(ctx).get_operand(0), op))
}

/// Operations during which a view may be detached: calls, awaits and BYOB reads.
fn may_detach(ctx: &Context, op: Ptr<Operation>) -> bool {
    crate::is_any::<(crate::foreign::ForeignOp, CallOp, AsyncCallOp, AwaitOp)>(ctx, op)
        || intrinsic(ctx, op) == Some(Intrinsic::ReaderReadByob)
}

/// A proof that `index < length` holds in the blocks `target` dominates.
struct Fact {
    target: Ptr<BasicBlock>,
    length: Length,
}

/// Fold stable array lengths and replace accesses proven in bounds with unchecked operations.
pub(crate) fn prove_bounds(
    root: Ptr<Operation>,
    ctx: &mut Context,
    numeric: &crate::facts::ProgramFacts,
    dominance: &mut DomInfo,
) -> Result<IRStatus> {
    let mut status = IRStatus::Unchanged;
    for function in functions(root, ctx) {
        let facts = facts(ctx, numeric, &function);
        let mut intervals = GuardedRanges::new(ctx, numeric, &function);
        let mut lengths = Vec::new();
        let mut accesses = Vec::new();
        let region = function.get_region(ctx);
        for (block, op) in region
            .deref(ctx)
            .iter(ctx)
            .flat_map(|block| block.deref(ctx).iter(ctx).map(move |op| (block, op)))
        {
            if Operation::get_op::<ArrayLenOp>(op, ctx).is_some() {
                lengths.push(op);
                continue;
            }
            let kind = if crate::is_any::<(ArrayGetOp, ArraySetOp)>(ctx, op) {
                Object::Array
            } else if intrinsic(ctx, op) == Some(Intrinsic::BytesGet) {
                Object::Bytes
            } else if matches!(
                intrinsic(ctx, op),
                Some(Intrinsic::StringCharAt | Intrinsic::StringIndex)
            ) {
                Object::String
            } else {
                continue;
            };
            let (object, index) = {
                let operation = op.deref(ctx);
                (operation.get_operand(0), operation.get_operand(1))
            };
            if Operation::get_op::<ArrayGetOp>(op, ctx).is_some() {
                let array = TypedHandle::<ArrayType>::from_handle(object.get_type(ctx), ctx)?;
                let element = array.deref(ctx).element();
                if crate::optional::optional_value(ctx, element).is_some() {
                    continue;
                }
            }
            let index_bounds = if kind != Object::String {
                intervals.at(dominance, op, index)
            } else {
                None
            };
            if !numeric.is_count(ctx, &function, index)
                && !index_bounds.is_some_and(|(lower, _)| lower >= 0)
            {
                continue;
            }
            let proven = (kind == Object::Array
                && index_bounds.is_some_and(|(lower, upper)| {
                    lower >= 0
                        && numeric
                            .array_length(&function, object)
                            .is_some_and(|(length, _)| upper < i128::from(length))
                }))
                || (kind == Object::Bytes
                    && index_bounds.is_some_and(|(lower, upper)| {
                        lower >= 0
                            && bytes::extent(ctx, numeric, object, op, 32)
                                .is_some_and(|(length, _)| upper < length)
                    }))
                || facts
                    .get(&(object, index, kind))
                    .into_iter()
                    .flatten()
                    .any(|fact| {
                        (fact.target == block
                            || dominance.block_strictly_dominates_block(ctx, fact.target, block))
                            && stable_length(ctx, numeric, &function, fact, op)
                    });
            if proven {
                accesses.push((op, object, index, kind));
            }
        }
        for (op, object, index, kind) in accesses {
            unchecked(ctx, op, object, index, kind)?;
            status = IRStatus::Changed;
        }
        for op in lengths {
            let array = op.deref(ctx).get_operand(0);
            if let Some((minimum, maximum)) = numeric.array_length(&function, array)
                && minimum == maximum
            {
                let ty = IntegerType::get(ctx, 32, Signedness::Signless);
                let attr = IntegerAttr::new(ty, APInt::from_u64(minimum.into(), bw(32)));
                let constant = ConstantOp::new(ctx, Box::new(attr));
                constant.get_operation().insert_before(ctx, op);
                let result = op.deref(ctx).get_result(0);
                result.replace_all_uses_with(ctx, &constant.get_result(ctx));
                Operation::erase(op, ctx);
                status = IRStatus::Changed;
            }
        }
    }
    Ok(status)
}

fn facts(
    ctx: &Context,
    numeric: &crate::facts::ProgramFacts,
    function: &FuncOp,
) -> HashMap<(Value, Value, Object), Vec<Fact>> {
    let mut facts: HashMap<_, Vec<_>> = HashMap::new();
    for block in function.get_region(ctx).deref(ctx).iter(ctx) {
        // Counts and lengths are never NaN, so false edges order them too.
        for order in orders(ctx, block).into_iter().flatten() {
            if !order.strict || !numeric.is_count(ctx, function, order.smaller) {
                continue;
            }
            let Some(length) = length(ctx, order.larger) else {
                continue;
            };
            facts
                .entry((length.object(), order.smaller, length.kind()))
                .or_default()
                .push(Fact {
                    target: order.target,
                    length,
                });
        }
    }
    facts
}

/// Array shrinking and byte detachment invalidate the length observed by a branch.
fn stable_length(
    ctx: &Context,
    numeric: &crate::facts::ProgramFacts,
    function: &FuncOp,
    fact: &Fact,
    consumer: Ptr<Operation>,
) -> bool {
    let read = match fact.length {
        Length::Array(_, read) | Length::Bytes(_, read) => read,
        Length::String(_) => return true,
    };
    let mut array_invalidated = matches!(fact.length, Length::Array(..))
        .then(|| numeric.array_length_invalidation(ctx, function, fact.length.object()));
    crate::facts::paths::preserves(ctx, read, consumer, (), |op, ()| {
        let invalidated = array_invalidated
            .as_mut()
            .map_or_else(|| may_detach(ctx, op), |invalidates| invalidates(op));
        (!invalidated).then_some(())
    })
}

/// Replace an access, preserving the checked read's optional result type.
fn unchecked(
    ctx: &mut Context,
    op: Ptr<Operation>,
    object: Value,
    index: Value,
    kind: Object,
) -> Result<()> {
    let target = IntegerType::get(ctx, 32, Signedness::Unsigned).into();
    let convert = NumericCastOp::new(ctx, index, target).get_operation();
    convert.insert_before(ctx, op);
    let index = convert.deref(ctx).get_result(0);
    if Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
        let value = op.deref(ctx).get_operand(2);
        let store = ArrayStoreOp::new(ctx, object, index, value).get_operation();
        store.insert_before(ctx, op);
        crate::exceptions::clear_caught(ctx, op);
        Operation::erase(op, ctx);
        return Ok(());
    }
    if kind == Object::String {
        let read = IntrinsicOp::new(ctx, Intrinsic::StringCharAtIndex, vec![object, index])
            .get_operation();
        read.insert_before(ctx, op);
        let mut character = read.deref(ctx).get_result(0);
        let result = op.deref(ctx).get_result(0);
        if crate::optional::optional_value(ctx, result.get_type(ctx)).is_some() {
            let wrap = WrapOp::new(ctx, character);
            wrap.get_operation().insert_before(ctx, op);
            character = wrap.get_result(ctx);
        }
        result.replace_all_uses_with(ctx, &character);
        Operation::erase(op, ctx);
        return Ok(());
    }
    let read = if kind == Object::Bytes {
        IntrinsicOp::new(ctx, Intrinsic::BytesAt, vec![object, index]).get_operation()
    } else {
        ArrayAtOp::new(ctx, object, index)?.get_operation()
    };
    read.insert_before(ctx, op);
    let element = read.deref(ctx).get_result(0);
    let wrap = WrapOp::new(ctx, element);
    let wrap_op = wrap.get_operation();
    wrap_op.insert_before(ctx, op);
    let wrapped = wrap.get_result(ctx);
    let result = op.deref(ctx).get_result(0);
    result.replace_all_uses_with(ctx, &wrapped);
    Operation::erase(op, ctx);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pliron::{
        builtin::{
            attributes::IntegerAttr,
            ops::ConstantOp,
            types::{IntegerType, Signedness},
        },
        utils::apint::{APInt, bw},
    };

    #[test]
    fn converted_integer_constant_is_not_a_length() {
        let ctx = &mut Context::new();
        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(3, bw(32)));
        let integer = ConstantOp::new(ctx, Box::new(attr)).get_result(ctx);
        let number = NumericCastOp::new(ctx, integer, FP64Type::get(ctx).into()).get_result(ctx);
        assert!(length(ctx, number).is_none());
    }
}
