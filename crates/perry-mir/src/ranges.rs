//! Integer ranges (`TODOs.md` P3.5): reads proven in bounds lose their check.
//!
//! The shared numeric analysis supplies non-negative integer ranges, including widened
//! loop-carried counts. This pass retains object bounds and buffer-detachment legality.
//!
//! A conditional branch on `i < length(a)` (or `length(a) > i`, or the negations on the false
//! edge) with `i` a count proves `0 <= i < length(a)` in the blocks its edge's target
//! dominates, when that target has no other predecessor. On every path to such a block, the
//! last execution of the target comes after the last definition of `i`, so the proof holds
//! for the current value of `i`. There, `a[i]` becomes an unchecked read of the element:
//!
//! - arrays never shrink (writes stay within `[0, length]` and `push` appends), so a proven
//!   index stays in range;
//! - a `Uint8Array` shrinks to length 0 when its buffer is transferred (a BYOB read), which
//!   happens only during a call or an `await`. Between the length read and the element read,
//!   no operation may call, await or read into a view; the length must be read in the block
//!   of the branch.
//!
//! The read's result is wrapped back into `T | undefined`; `fold` then removes the
//! `undefined` tests and coercions of the wrapped value.
//!
//! Strings are immutable, so `s.charAt(i)` under a proof of `i < s.length` (a scalar count)
//! becomes `string_char_at_index` without integrality or range checks.
use crate::{
    aggregates::{functions, operations},
    attrs::CompareKind,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    collections::{ArrayAtOp, ArrayGetOp, ArrayLenOp, StringLenOp},
    control::CondBrOp,
    intrinsics::{Intrinsic, IntrinsicOp},
    optional::WrapOp,
    scalar::{CompareOp, NumericCastOp},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{OneRegionInterface, OneResultInterface},
        ops::FuncOp,
        types::{FP64Type, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    graph::dominance::DomInfo,
    irbuild::IRStatus,
    linked_list::ContainsLinkedList,
    linked_list::LinkedList,
    op::Op,
    operation::Operation,
    r#type::Typed,
    value::Value,
};
use std::collections::{HashMap, HashSet};

/// What a length is the length of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Length {
    Array(Value),
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
            Length::Array(value) | Length::Bytes(value, _) | Length::String(value) => value,
        }
    }

    fn kind(self) -> Object {
        match self {
            Length::Array(_) => Object::Array,
            Length::Bytes(..) => Object::Bytes,
            Length::String(_) => Object::String,
        }
    }
}

fn intrinsic(ctx: &Context, op: Ptr<Operation>) -> Option<Intrinsic> {
    Operation::get_op::<IntrinsicOp>(op, ctx).and_then(|i| i.intrinsic(ctx))
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
        return Some(Length::Array(len.deref(ctx).get_operand(0)));
    }
    (intrinsic(ctx, op) == Some(Intrinsic::BytesLength))
        .then(|| Length::Bytes(op.deref(ctx).get_operand(0), op))
}

/// Operations during which a view may be detached: calls, awaits and BYOB reads.
fn may_detach(ctx: &Context, op: Ptr<Operation>) -> bool {
    Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
        || Operation::get_op::<CallOp>(op, ctx).is_some()
        || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
        || Operation::get_op::<AwaitOp>(op, ctx).is_some()
        || intrinsic(ctx, op) == Some(Intrinsic::ReaderReadByob)
}

struct Function {
    blocks: Vec<Ptr<BasicBlock>>,
    successors: HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
    predecessors: HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
}

impl Function {
    fn new(ctx: &Context, function: &FuncOp) -> Self {
        let blocks: Vec<Ptr<BasicBlock>> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
        let mut successors: HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = HashMap::new();
        let mut predecessors: HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = HashMap::new();
        for &block in &blocks {
            let Some(terminator) = block.deref(ctx).get_terminator(ctx) else {
                continue;
            };
            let dests: Vec<Ptr<BasicBlock>> = terminator.deref(ctx).successors().collect();
            for dest in dests {
                successors.entry(block).or_default().push(dest);
                predecessors.entry(dest).or_default().push(block);
            }
        }
        Function {
            blocks,
            successors,
            predecessors,
        }
    }

    /// Blocks reachable from `from` (included) along edges that do not enter `avoid`.
    fn reach(
        &self,
        from: Ptr<BasicBlock>,
        avoid: Ptr<BasicBlock>,
        edges: &HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
    ) -> HashSet<Ptr<BasicBlock>> {
        let mut seen = HashSet::from([from]);
        let mut work = vec![from];
        while let Some(block) = work.pop() {
            for &next in edges.get(&block).into_iter().flatten() {
                if next != avoid && seen.insert(next) {
                    work.push(next);
                }
            }
        }
        seen
    }
}

/// A proof that `index < length` holds in the blocks `target` dominates.
struct Fact {
    branch: Ptr<BasicBlock>,
    target: Ptr<BasicBlock>,
    index: Value,
    length: Length,
}

/// Replace reads proven in bounds with unchecked reads; see the module documentation.
pub(crate) fn prove_bounds(
    root: Ptr<Operation>,
    ctx: &mut Context,
    numeric: &crate::numeric::NumericAnalysis,
    dominance: &mut DomInfo,
) -> IRStatus {
    let mut status = IRStatus::Unchanged;
    for function in functions(root, ctx) {
        let info = Function::new(ctx, &function);
        let counts: HashSet<_> = info
            .blocks
            .iter()
            .flat_map(|block| {
                block
                    .deref(ctx)
                    .arguments()
                    .chain(
                        block
                            .deref(ctx)
                            .iter(ctx)
                            .flat_map(|op| op.deref(ctx).results().collect::<Vec<_>>()),
                    )
                    .collect::<Vec<_>>()
            })
            .filter(|&value| numeric.is_count(ctx, &function, value))
            .collect();
        let facts = facts(ctx, &info, &counts);
        if facts.is_empty() {
            continue;
        }
        for op in operations(ctx, &function) {
            let kind = if Operation::get_op::<ArrayGetOp>(op, ctx).is_some() {
                Object::Array
            } else if intrinsic(ctx, op) == Some(Intrinsic::BytesGet) {
                Object::Bytes
            } else if intrinsic(ctx, op) == Some(Intrinsic::StringCharAt) {
                Object::String
            } else {
                continue;
            };
            let (object, index) = {
                let operation = op.deref(ctx);
                (operation.get_operand(0), operation.get_operand(1))
            };
            let bytes = kind == Object::Bytes;
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
            if !counts.contains(&index) {
                continue;
            }
            let block = op.deref(ctx).get_parent_block().expect("in a block");
            let proven = facts
                .get(&(object, index, kind))
                .into_iter()
                .flatten()
                .any(|fact| {
                    (fact.target == block
                        || dominance.block_strictly_dominates_block(ctx, fact.target, block))
                        && (!bytes || stays_attached(ctx, &info, fact, block))
                });
            if proven {
                unchecked(ctx, op, object, index, kind);
                status = IRStatus::Changed;
            }
        }
    }
    status
}

fn facts(
    ctx: &Context,
    info: &Function,
    counts: &HashSet<Value>,
) -> HashMap<(Value, Value, Object), Vec<Fact>> {
    let mut facts = HashMap::new();
    for &block in &info.blocks {
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
        let single = info
            .predecessors
            .get(&target)
            .is_some_and(|preds| preds.len() == 1);
        if target == other || !single || !counts.contains(&index) {
            continue;
        }
        let Some(length) = length(ctx, len) else {
            continue;
        };
        facts
            .entry((length.object(), index, length.kind()))
            .or_insert_with(Vec::new)
            .push(Fact {
                branch: block,
                target,
                index,
                length,
            });
    }
    facts
}

/// Whether no operation between `fact`'s length read and a read in `block` may detach the
/// view (see the module documentation).
fn stays_attached(ctx: &Context, info: &Function, fact: &Fact, block: Ptr<BasicBlock>) -> bool {
    let Length::Bytes(_, read) = fact.length else {
        return true;
    };
    if read.deref(ctx).get_parent_block() != Some(fact.branch) {
        return false;
    }
    let mut after = read.deref(ctx).get_next();
    while let Some(op) = after {
        if may_detach(ctx, op) {
            return false;
        }
        after = op.deref(ctx).get_next();
    }
    // Paths from the branch's target to the read that do not redefine the index.
    let Some(defined) = fact.index.get_defining_block(ctx) else {
        return false;
    };
    let forward = info.reach(fact.target, defined, &info.successors);
    let backward = info.reach(block, defined, &info.predecessors);
    let mut on_paths: HashSet<Ptr<BasicBlock>> = forward.intersection(&backward).copied().collect();
    on_paths.extend([fact.target, block]);
    on_paths
        .iter()
        .all(|&on_path| on_path.deref(ctx).iter(ctx).all(|op| !may_detach(ctx, op)))
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
        let character = read.deref(ctx).get_result(0);
        let result = op.deref(ctx).get_result(0);
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
