//! Integer range analysis for eliding bounds checks on proven in-bounds reads.
//!
//! The shared numeric analysis supplies non-negative integer ranges, including widened
//! loop-carried counts. This pass retains object bounds and buffer-detachment legality.
//!
//! A conditional branch on `i < length(a)` (or `length(a) > i`, or the negations on the false
//! edge) with a non-negative integer `i` proves `0 <= i < length(a)` in the blocks dominated
//! by that edge's target when the target has a single predecessor. On every path to such a
//! block, the last execution of the target follows the last definition of `i`, so the proof
//! holds for the current value of `i`. Within those blocks, `a[i]` rewrites to an unchecked
//! element read under these conditions:
//!
//! - Arrays retain a proven index when every operation between the length read and the element
//!   read preserves the array length, using converged mutation and object-identity facts for calls.
//! - A `Uint8Array` shrinks to length 0 only when its buffer is transferred during a BYOB read,
//!   which requires a call or an `await`. The proof holds when the path between the length read
//!   and the element read is free of calls, awaits, and view reads.
//!
//! The unchecked read's result is wrapped back into `T | undefined`, and `fold` subsequently
//! eliminates redundant `undefined` tests and coercions on the wrapped value.
//!
//! Strings are immutable, so `s.charAt(i)` under a proof of `i < s.length` rewrites to
//! `string_char_at_index`.
use crate::{
    aggregates::{functions, operations},
    attrs::CompareKind,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    collections::{ArrayAtOp, ArrayGetOp, ArrayLenOp, StringLenOp},
    control::CondBrOp,
    intrinsics::{Intrinsic, IntrinsicOp, intrinsic_of as intrinsic},
    optional::WrapOp,
    scalar::{CompareOp, NumericCastOp},
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
    r#type::Typed,
    utils::apint::{APInt, bw},
    value::Value,
};
use std::collections::HashMap;

/// What a length is the length of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Length {
    Array(Value, Ptr<Operation>),
    /// A view, and the operation that read its length.
    Bytes(Value, Ptr<Operation>),
    /// A string's scalar count.
    String(Value),
}

/// What an indexed read reads: facts and reads match by kind and object.
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

/// Fold stable array lengths and replace reads proven in bounds with unchecked reads.
pub(crate) fn prove_bounds(
    root: Ptr<Operation>,
    ctx: &mut Context,
    numeric: &crate::facts::ProgramFacts,
    dominance: &mut DomInfo,
) -> IRStatus {
    let mut status = IRStatus::Unchanged;
    for function in functions(root, ctx) {
        let facts = facts(ctx, numeric, &function);
        let mut lengths = Vec::new();
        for op in operations(ctx, &function) {
            if Operation::get_op::<ArrayLenOp>(op, ctx).is_some() {
                lengths.push(op);
                continue;
            }
            let kind = if Operation::get_op::<ArrayGetOp>(op, ctx).is_some() {
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
            if kind == Object::Array {
                let array = object.get_type(ctx);
                let array = array.deref(ctx);
                let element = array
                    .downcast_ref::<crate::types::ArrayType>()
                    .expect("array")
                    .element();
                if crate::optional::optional_value(ctx, element).is_some() {
                    continue;
                }
            }
            if !numeric.is_count(ctx, &function, index) {
                continue;
            }
            let block = op.deref(ctx).get_parent_block().expect("in a block");
            let proven = (kind == Object::Array
                && numeric.array_in_bounds(ctx, &function, object, index))
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
                unchecked(ctx, op, object, index, kind);
                status = IRStatus::Changed;
            }
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
    status
}

fn facts(
    ctx: &Context,
    numeric: &crate::facts::ProgramFacts,
    function: &FuncOp,
) -> HashMap<(Value, Value, Object), Vec<Fact>> {
    let mut facts = HashMap::new();
    for block in function.get_region(ctx).deref(ctx).iter(ctx) {
        let Some(terminator) = block.deref(ctx).get_terminator(ctx) else {
            continue;
        };
        let Some(cond_br) = Operation::get_op::<CondBrOp>(terminator, ctx) else {
            continue;
        };
        let condition = cond_br.get_operand_condition(ctx);
        let Some(compare) = condition
            .defining_op()
            .and_then(|op| Operation::get_op::<CompareOp>(op, ctx))
        else {
            continue;
        };
        let operation = compare.get_operation().deref(ctx);
        let (x, y) = (operation.get_operand(0), operation.get_operand(1));
        // (successor, index, length): counts and lengths are never NaN, so the false edge of
        // `x >= y` proves `x < y` too.
        let (succ, index, len) = match compare.kind(ctx) {
            CompareKind::Lt => (0, x, y),
            CompareKind::Gt => (0, y, x),
            CompareKind::Ge => (1, x, y),
            CompareKind::Le => (1, y, x),
            CompareKind::Eq | CompareKind::Ne => continue,
        };
        let target = terminator.deref(ctx).get_successor(succ);
        let other = terminator.deref(ctx).get_successor(1 - succ);
        let single = target.num_preds(ctx) == 1;
        if target == other || !single || !numeric.is_count(ctx, function, index) {
            continue;
        }
        let Some(length) = length(ctx, len) else {
            continue;
        };
        facts
            .entry((length.object(), index, length.kind()))
            .or_insert_with(Vec::new)
            .push(Fact { target, length });
    }
    facts
}

/// Array mutation and byte detachment invalidate the length observed by a branch.
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

/// Turn the checked read `op` into an unchecked one, wrapped back into `T | undefined`
/// (a character is a plain string).
fn unchecked(ctx: &mut Context, op: Ptr<Operation>, object: Value, index: Value, kind: Object) {
    let target = IntegerType::get(ctx, 32, Signedness::Unsigned).into();
    let convert = NumericCastOp::new(ctx, index, target).get_operation();
    convert.insert_before(ctx, op);
    let index = convert.deref(ctx).get_result(0);
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
        return;
    }
    let read = if kind == Object::Bytes {
        IntrinsicOp::new(ctx, Intrinsic::BytesAt, vec![object, index]).get_operation()
    } else {
        ArrayAtOp::new(ctx, object, index)
            .expect("an array")
            .get_operation()
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
