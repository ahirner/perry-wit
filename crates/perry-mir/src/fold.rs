//! Conservative scalar, optional-value and constant-branch folding.
//!
//! - Tests and coercions of a wrapped value: `is_undefined(wrap x)` is false,
//!   `is_undefined(undefined)` true, and `to_number`, `to_string` and `unwrap` of `wrap x`
//!   are `x` (an unwrap whose throw is caught stays, since `perry.caught` follows it).
//! - Arithmetic and comparisons of constants. `%` is `fmod`, as in Rust.
//! - A comparison of two `i32`s converted to numbers compares the `i32`s (the conversion is
//!   exact and monotone).
//! - A conditional branch on a constant becomes a branch to the taken successor.
//!
//! Branch folding removes unreachable blocks immediately to preserve dominance. DCE and
//! `simplify_cfg` remove unused values and merge the remaining trivial blocks.
use crate::{
    aggregates::{functions, operations},
    attrs::{BinaryKind, CompareKind, ConvertKind},
    control::{BrOp, CondBrOp},
    exceptions::CaughtOp,
    optional::{IsUndefinedOp, ToNumberOp, ToStringOp, UndefinedOp, UnwrapOp, WrapOp},
    scalar::{BinaryOp, CompareOp, ConvertOp},
};
use pliron::{
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
        op_interfaces::BranchOpInterface,
        ops::ConstantOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    irbuild::IRStatus,
    linked_list::LinkedList,
    op::{Op, op_cast},
    operation::Operation,
    r#type::Typed,
    utils::apint::{APInt, bw},
    value::Value,
};

/// A constant operand: a number or a boolean.
#[derive(Clone, Copy)]
enum Constant {
    Number(f64),
    Bool(bool),
}

fn constant(ctx: &Context, value: Value) -> Option<Constant> {
    let op = value.defining_op()?;
    let constant = Operation::get_op::<ConstantOp>(op, ctx)?;
    let attr = constant.get_attr_builtin_constant_value(ctx)?;
    if let Some(double) = attr.downcast_ref::<FPDoubleAttr>() {
        return Some(Constant::Number(f64::from(double.clone())));
    }
    let integer = attr.downcast_ref::<IntegerAttr>()?;
    (integer.get_type().deref(ctx).width() == 1).then(|| Constant::Bool(!integer.value().is_zero()))
}

fn wrapped(ctx: &Context, value: Value) -> Option<Value> {
    let op = value.defining_op()?;
    Operation::get_op::<WrapOp>(op, ctx)?;
    Some(op.deref(ctx).get_operand(0))
}

fn converted_i32(ctx: &Context, value: Value) -> Option<Value> {
    let op = value.defining_op()?;
    let convert = Operation::get_op::<ConvertOp>(op, ctx)?;
    (convert.kind(ctx) == ConvertKind::I32ToF64).then(|| op.deref(ctx).get_operand(0))
}

fn compare<T: PartialOrd>(kind: CompareKind, a: T, b: T) -> bool {
    match kind {
        CompareKind::Lt => a < b,
        CompareKind::Le => a <= b,
        CompareKind::Gt => a > b,
        CompareKind::Ge => a >= b,
        CompareKind::Eq => a == b,
        CompareKind::Ne => a != b,
    }
}

/// Insert a constant before `op` and return its value.
fn insert_constant(ctx: &mut Context, op: Ptr<Operation>, value: Constant) -> Value {
    let constant = match value {
        Constant::Number(number) => ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(number))),
        Constant::Bool(flag) => {
            let ty = IntegerType::get(ctx, 1, Signedness::Signless);
            let attr = IntegerAttr::new(ty, APInt::from_u64(u64::from(flag), bw(1)));
            ConstantOp::new(ctx, Box::new(attr))
        }
    };
    let constant = constant.get_operation();
    constant.insert_before(ctx, op);
    constant.deref(ctx).get_result(0)
}

/// What `op` folds to.
enum Folded {
    Value(Value),
    Constant(Constant),
    /// A comparison of the operands of two conversions.
    Compare(CompareKind, Value, Value),
    /// A branch to successor `n`.
    Branch(usize),
}

fn fold_op(ctx: &Context, op: Ptr<Operation>) -> Option<Folded> {
    let operation = op.deref(ctx);
    if Operation::get_op::<IsUndefinedOp>(op, ctx).is_some() {
        let operand = operation.get_operand(0);
        if wrapped(ctx, operand).is_some() {
            return Some(Folded::Constant(Constant::Bool(false)));
        }
        let undefined = operand
            .defining_op()
            .is_some_and(|def| Operation::get_op::<UndefinedOp>(def, ctx).is_some());
        return undefined.then_some(Folded::Constant(Constant::Bool(true)));
    }
    if Operation::get_op::<ToNumberOp>(op, ctx).is_some()
        || Operation::get_op::<ToStringOp>(op, ctx).is_some()
    {
        let inner = wrapped(ctx, operation.get_operand(0))?;
        let same = inner.get_type(ctx) == operation.get_result(0).get_type(ctx);
        return same.then_some(Folded::Value(inner));
    }
    if Operation::get_op::<UnwrapOp>(op, ctx).is_some() {
        let caught = operation
            .get_next()
            .is_some_and(|next| Operation::get_op::<CaughtOp>(next, ctx).is_some());
        return (!caught)
            .then(|| wrapped(ctx, operation.get_operand(0)))
            .flatten()
            .map(Folded::Value);
    }
    if let Some(binary) = Operation::get_op::<BinaryOp>(op, ctx) {
        let (Some(Constant::Number(a)), Some(Constant::Number(b))) = (
            constant(ctx, operation.get_operand(0)),
            constant(ctx, operation.get_operand(1)),
        ) else {
            return None;
        };
        let value = match binary.kind(ctx) {
            BinaryKind::Add => a + b,
            BinaryKind::Sub => a - b,
            BinaryKind::Mul => a * b,
            BinaryKind::Div => a / b,
            BinaryKind::Rem => a % b,
        };
        return Some(Folded::Constant(Constant::Number(value)));
    }
    if let Some(cmp) = Operation::get_op::<CompareOp>(op, ctx) {
        let kind = cmp.kind(ctx);
        let (a, b) = (operation.get_operand(0), operation.get_operand(1));
        return match (constant(ctx, a), constant(ctx, b)) {
            (Some(Constant::Number(x)), Some(Constant::Number(y))) => {
                Some(Folded::Constant(Constant::Bool(compare(kind, x, y))))
            }
            (Some(Constant::Bool(x)), Some(Constant::Bool(y))) => {
                Some(Folded::Constant(Constant::Bool(compare(kind, x, y))))
            }
            _ => match (converted_i32(ctx, a), converted_i32(ctx, b)) {
                (Some(x), Some(y)) => Some(Folded::Compare(kind, x, y)),
                _ => None,
            },
        };
    }
    if let Some(cond_br) = Operation::get_op::<CondBrOp>(op, ctx)
        && let Some(Constant::Bool(taken)) = constant(ctx, cond_br.get_operand_condition(ctx))
    {
        return Some(Folded::Branch(if taken { 0 } else { 1 }));
    }
    None
}

/// Fold every operation of `root`'s functions once; see the module documentation.
pub fn fold(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    let mut status = IRStatus::Unchanged;
    let mut changed_branches = false;
    for function in functions(root, ctx) {
        for op in operations(ctx, &function) {
            let Some(folded) = fold_op(ctx, op) else {
                continue;
            };
            status = IRStatus::Changed;
            match folded {
                Folded::Branch(succ) => {
                    changed_branches = true;
                    let dyn_op = Operation::get_op_dyn(op, ctx);
                    let branch = op_cast::<dyn BranchOpInterface>(dyn_op.as_ref())
                        .expect("cond_br branches");
                    let args = branch.successor_operands(ctx, succ);
                    let dest = op.deref(ctx).get_successor(succ);
                    let br = BrOp::new(ctx, dest, args).get_operation();
                    br.insert_before(ctx, op);
                    Operation::erase(op, ctx);
                }
                Folded::Value(value) => {
                    let result = op.deref(ctx).get_result(0);
                    result.replace_all_uses_with(ctx, &value);
                    Operation::erase(op, ctx);
                }
                Folded::Constant(value) => {
                    let value = insert_constant(ctx, op, value);
                    let result = op.deref(ctx).get_result(0);
                    result.replace_all_uses_with(ctx, &value);
                    Operation::erase(op, ctx);
                }
                Folded::Compare(kind, a, b) => {
                    let compare = CompareOp::new(ctx, kind, a, b).get_operation();
                    compare.insert_before(ctx, op);
                    let value = compare.deref(ctx).get_result(0);
                    let result = op.deref(ctx).get_result(0);
                    result.replace_all_uses_with(ctx, &value);
                    Operation::erase(op, ctx);
                }
            }
        }
    }
    if changed_branches {
        pliron::opts::simplify_cfg::remove_blocks_inside_op(
            root,
            ctx,
            &mut pliron::irbuild::rewriter::IRRewriter::<pliron::irbuild::listener::DummyListener>::default(),
        );
    }
    status
}
