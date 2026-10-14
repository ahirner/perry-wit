use std::collections::HashMap;

use pliron::basic_block::BasicBlock;
use pliron::builtin::attributes::{FPDoubleAttr, IntegerAttr};
use pliron::builtin::op_interfaces::{
    OneRegionInterface, OneResultInterface, SingleBlockRegionInterface,
};
use pliron::builtin::ops::{ConstantOp, FuncOp, ModuleOp};
use pliron::builtin::types::{FP64Type, FunctionType, IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::irbuild::IRStatus;
use pliron::linked_list::{ContainsLinkedList, LinkedList};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::pass::{AnalysisManager, Pass, PassResult};
use pliron::result::Result;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use pliron::utils::apint::{APInt, bw};
use pliron::value::Value;

use crate::attrs::{Absence, BinaryKind, CompareKind, ErrorKind};
use crate::calls::CallOp;
use crate::collections::{ArrayAtOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, StringConstOp};
use crate::control::{BrOp, CondBrOp, ReturnOp};
use crate::exceptions::{CaughtOp, ErrorNewOp, RaiseOp, RethrowOp};
use crate::heap::{AllocOp, FieldGetOp};
use crate::intrinsics::{Intrinsic, IntrinsicOp, JsonStringifyOp};
use crate::optional::{
    IsNullOp, IsUndefinedOp, NullOp, UndefinedOp, UnwrapOp, WrapOp, absence, optional_value,
};
use crate::scalar::{BinaryOp, CompareOp};
use crate::types::{ArrayType, JsonType, RecordType, StringType, VariantType};
use crate::variants::{VariantNewOp, VariantPayloadOp, VariantTagOp};

/// MIR pass that recognizes `JSON.parse<T>(JSON.stringify(value))` and rewrites
/// provably compatible schemas into a schema-driven deep-copy/normalization helper
/// without constructing intermediate JSON text or a JSON cursor.
pub struct NormalizeJsonCopy;

impl Pass for NormalizeJsonCopy {
    fn name(&self) -> &str {
        "normalize-json-copy"
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        _analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let Some(module) = Operation::get_op::<ModuleOp>(root, ctx) else {
            return Ok(PassResult::default());
        };

        let mut rewrites = 0u64;
        let mut missed = 0u64;
        let mut helper_cache: HashMap<(TypeHandle, TypeHandle), Identifier> = HashMap::new();
        let mut next_helper_id = 0usize;

        let funcs: Vec<FuncOp> = module
            .get_body(ctx, 0)
            .deref(ctx)
            .iter(ctx)
            .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
            .collect();

        for func in funcs {
            let sites = find_parse_stringify_sites(func, ctx);
            for site in sites {
                let source_ty = site.source_value.get_type(ctx);
                let target_ty = site.target_ty;
                if can_rewrite_parse_stringify(ctx, &site, source_ty, target_ty) {
                    let helper_sym = if let Some(sym) =
                        helper_cache.get(&(source_ty, target_ty)).cloned()
                    {
                        sym
                    } else {
                        let sym: Identifier = format!("perry_json_copy_{next_helper_id}")
                            .try_into()
                            .unwrap();
                        next_helper_id += 1;
                        build_json_copy_helper(ctx, module, sym.clone(), source_ty, target_ty)?;
                        helper_cache.insert((source_ty, target_ty), sym.clone());
                        sym
                    };

                    rewrite_site(ctx, &site, helper_sym);
                    rewrites += 1;
                } else {
                    missed += 1;
                }
            }
        }

        if rewrites > 0 || missed > 0 {
            let mut stats = crate::pipeline::statistics(root, ctx);
            stats.parse_stringify_rewrites += rewrites;
            stats.parse_stringify_missed += missed;
            root.deref_mut(ctx)
                .attributes
                .set(crate::pipeline::STATISTICS.clone(), stats);
        }

        let mut result = PassResult::default();
        result.ir_changed = if rewrites == 0 {
            IRStatus::Unchanged
        } else {
            IRStatus::Changed
        };
        Ok(result)
    }
}

struct ParseStringifySite {
    stringify_op: Ptr<Operation>,
    unwrap_op: Option<Ptr<Operation>>,
    consumer_op: Ptr<Operation>,
    intermediate_parse_op: Option<Ptr<Operation>>,
    source_value: Value,
    target_ty: TypeHandle,
}

fn find_parse_stringify_sites(func: FuncOp, ctx: &Context) -> Vec<ParseStringifySite> {
    let mut sites = Vec::new();
    let mut consumed_parse_ops = std::collections::HashSet::new();
    let region = func.get_region(ctx);
    let blocks: Vec<Ptr<BasicBlock>> = region.deref(ctx).iter(ctx).collect();

    // First pass: find all typed/decoded consumers of JSON.stringify (either via
    // `perry_json_parse_*`, `perry_json_decode_*`, or `Intrinsic::JsonTo*`/`JsonRequireNumber`).
    for &block in &blocks {
        let ops: Vec<Ptr<Operation>> = block.deref(ctx).iter(ctx).collect();
        for op in ops {
            if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
                let callee = call.callee(ctx).to_string();
                if callee.starts_with("perry_json_parse_") {
                    let args: Vec<Value> = op.deref(ctx).operands().collect();
                    if args.len() != 1 || op.deref(ctx).get_num_results() != 1 {
                        continue;
                    }
                    let Some((stringify_op, unwrap_op, source_value)) =
                        match_unwrapped_stringify(ctx, args[0])
                    else {
                        continue;
                    };
                    let target_ty = op.deref(ctx).get_result(0).get_type(ctx);
                    sites.push(ParseStringifySite {
                        stringify_op,
                        unwrap_op,
                        consumer_op: op,
                        intermediate_parse_op: None,
                        source_value,
                        target_ty,
                    });
                } else if callee.starts_with("perry_json_decode_") {
                    let args: Vec<Value> = op.deref(ctx).operands().collect();
                    if args.len() != 1 || op.deref(ctx).get_num_results() != 1 {
                        continue;
                    }
                    let Some((parse_op, stringify_op, unwrap_op, source_value)) =
                        match_json_parse_of_stringify(ctx, args[0])
                    else {
                        continue;
                    };
                    consumed_parse_ops.insert(parse_op);
                    let target_ty = op.deref(ctx).get_result(0).get_type(ctx);
                    sites.push(ParseStringifySite {
                        stringify_op,
                        unwrap_op,
                        consumer_op: op,
                        intermediate_parse_op: Some(parse_op),
                        source_value,
                        target_ty,
                    });
                }
            } else if let Some(intrinsic_op) = Operation::get_op::<IntrinsicOp>(op, ctx)
                && matches!(
                    intrinsic_op.intrinsic(ctx),
                    Some(
                        Intrinsic::JsonRequireNumber
                            | Intrinsic::JsonToNumber
                            | Intrinsic::JsonToI64
                            | Intrinsic::JsonToU64
                            | Intrinsic::JsonToBool
                            | Intrinsic::JsonToString
                    )
                )
            {
                let args: Vec<Value> = op.deref(ctx).operands().collect();
                if args.len() != 1 || op.deref(ctx).get_num_results() != 1 {
                    continue;
                }
                let Some((parse_op, stringify_op, unwrap_op, source_value)) =
                    match_json_parse_of_stringify(ctx, args[0])
                else {
                    continue;
                };
                consumed_parse_ops.insert(parse_op);
                let target_ty = op.deref(ctx).get_result(0).get_type(ctx);
                sites.push(ParseStringifySite {
                    stringify_op,
                    unwrap_op,
                    consumer_op: op,
                    intermediate_parse_op: Some(parse_op),
                    source_value,
                    target_ty,
                });
            }
        }
    }

    // Second pass: any remaining `Intrinsic::JsonParse` directly over `JSON.stringify`
    // (e.g. untyped `JSON.parse(JSON.stringify(x))` or optional `JsonIsNull` branching)
    // is recorded as a site with `target_ty = JsonType` so `parse_stringify_missed` is tracked.
    for &block in &blocks {
        let ops: Vec<Ptr<Operation>> = block.deref(ctx).iter(ctx).collect();
        for op in ops {
            if consumed_parse_ops.contains(&op) {
                continue;
            }
            let Some(intrinsic_op) = Operation::get_op::<IntrinsicOp>(op, ctx) else {
                continue;
            };
            if intrinsic_op.intrinsic(ctx) != Some(Intrinsic::JsonParse) {
                continue;
            }
            let args: Vec<Value> = op.deref(ctx).operands().collect();
            if args.len() != 1 || op.deref(ctx).get_num_results() != 1 {
                continue;
            }
            let Some((stringify_op, unwrap_op, source_value)) =
                match_unwrapped_stringify(ctx, args[0])
            else {
                continue;
            };
            let target_ty = op.deref(ctx).get_result(0).get_type(ctx);
            sites.push(ParseStringifySite {
                stringify_op,
                unwrap_op,
                consumer_op: op,
                intermediate_parse_op: None,
                source_value,
                target_ty,
            });
        }
    }

    sites
}

type MatchedParseOfStringify = (Ptr<Operation>, Ptr<Operation>, Option<Ptr<Operation>>, Value);

fn match_json_parse_of_stringify(
    ctx: &Context,
    parsed_val: Value,
) -> Option<MatchedParseOfStringify> {
    let parse_op = parsed_val.defining_op()?;
    let parse_intr = Operation::get_op::<IntrinsicOp>(parse_op, ctx)?;
    if parse_intr.intrinsic(ctx) != Some(Intrinsic::JsonParse) {
        return None;
    }
    let parse_args: Vec<Value> = parse_op.deref(ctx).operands().collect();
    if parse_args.len() != 1 {
        return None;
    }
    let (stringify_op, unwrap_op, source_value) = match_unwrapped_stringify(ctx, parse_args[0])?;
    Some((parse_op, stringify_op, unwrap_op, source_value))
}

fn match_unwrapped_stringify(
    ctx: &Context,
    text_val: Value,
) -> Option<(Ptr<Operation>, Option<Ptr<Operation>>, Value)> {
    let def_op = text_val.defining_op()?;
    if Operation::get_op::<JsonStringifyOp>(def_op, ctx).is_some() {
        let stringify_args: Vec<Value> = def_op.deref(ctx).operands().collect();
        if stringify_args.len() != 1 {
            return None;
        }
        return Some((def_op, None, stringify_args[0]));
    }
    if Operation::get_op::<UnwrapOp>(def_op, ctx).is_some() {
        let unwrap_args: Vec<Value> = def_op.deref(ctx).operands().collect();
        if unwrap_args.len() != 1 {
            return None;
        }
        let stringify_op = unwrap_args[0].defining_op()?;
        Operation::get_op::<JsonStringifyOp>(stringify_op, ctx)?;
        let stringify_args: Vec<Value> = stringify_op.deref(ctx).operands().collect();
        if stringify_args.len() != 1 {
            return None;
        }
        return Some((stringify_op, Some(def_op), stringify_args[0]));
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyPosition {
    Root,
    RecordField,
    Element,
}

fn can_rewrite_parse_stringify(
    ctx: &Context,
    site: &ParseStringifySite,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
) -> bool {
    let stringify_res = site.stringify_op.deref(ctx).get_result(0);
    if stringify_res.uses(ctx).len() != 1 {
        return false;
    }
    if let Some(unwrap_op) = site.unwrap_op {
        let unwrap_res = unwrap_op.deref(ctx).get_result(0);
        if unwrap_res.uses(ctx).len() != 1 {
            return false;
        }
    }
    if let Some(parse_op) = site.intermediate_parse_op {
        let parse_res = parse_op.deref(ctx).get_result(0);
        if parse_res.uses(ctx).len() != 1 {
            return false;
        }
    }

    let consumer_res = site.consumer_op.deref(ctx).get_result(0);
    if value_reaches_stringify(ctx, consumer_res)
        && (schemas_drop_fields(ctx, source_ty, target_ty, 0)
            || value_may_have_json_residue(ctx, site.source_value))
    {
        return false;
    }

    schemas_compatible(ctx, source_ty, target_ty, 0)
}

fn type_may_contain_record(ctx: &Context, ty: TypeHandle, depth: usize) -> bool {
    if depth > 16 {
        return true;
    }
    let inner = optional_value(ctx, ty).unwrap_or(ty);
    if let Some(rec) = inner.deref(ctx).downcast_ref::<RecordType>() {
        if rec.is_tuple() {
            return rec
                .fields()
                .map(|fields| {
                    fields
                        .iter()
                        .any(|(_, elem)| type_may_contain_record(ctx, *elem, depth + 1))
                })
                .unwrap_or(true);
        }
        return true;
    }
    if inner.deref(ctx).downcast_ref::<JsonType>().is_some() {
        return true;
    }
    if let Some(arr) = inner.deref(ctx).downcast_ref::<ArrayType>() {
        return type_may_contain_record(ctx, arr.element(), depth + 1);
    }
    if let Some(var) = inner.deref(ctx).downcast_ref::<VariantType>() {
        return var.cases().iter().any(|(_, payload)| {
            payload.is_some_and(|payload| type_may_contain_record(ctx, payload, depth + 1))
        });
    }
    false
}

fn value_reaches_stringify(ctx: &Context, root: Value) -> bool {
    if !type_may_contain_record(ctx, root.get_type(ctx), 0) {
        return false;
    }
    let mut worklist = vec![root];
    let mut visited = std::collections::HashSet::new();
    while let Some(val) = worklist.pop() {
        if !visited.insert(val) {
            continue;
        }
        if !type_may_contain_record(ctx, val.get_type(ctx), 0) {
            continue;
        }
        for use_site in val.uses(ctx) {
            let user = use_site.user_op();
            if Operation::get_op::<JsonStringifyOp>(user, ctx).is_some() {
                return true;
            }
            if let Some(intr) = Operation::get_op::<IntrinsicOp>(user, ctx)
                && matches!(
                    intr.intrinsic(ctx),
                    Some(Intrinsic::JsonResidue | Intrinsic::JsonRetain)
                )
            {
                return true;
            }
            if let Some(call) = Operation::get_op::<CallOp>(user, ctx) {
                let callee = call.callee(ctx).to_string();
                if !callee.starts_with("perry_json_") {
                    return true;
                }
            }
            if Operation::get_op::<BrOp>(user, ctx).is_some()
                || Operation::get_op::<CondBrOp>(user, ctx).is_some()
            {
                for succ_idx in 0..user.deref(ctx).get_num_successors() {
                    let dest = user.deref(ctx).get_successor(succ_idx);
                    let arg_count = dest.deref(ctx).get_num_arguments();
                    for idx in 0..arg_count {
                        let arg = dest.deref(ctx).get_argument(idx);
                        if type_may_contain_record(ctx, arg.get_type(ctx), 0) {
                            worklist.push(arg);
                        }
                    }
                }
            }
            let num_res = user.deref(ctx).get_num_results();
            for idx in 0..num_res {
                let res = user.deref(ctx).get_result(idx);
                if type_may_contain_record(ctx, res.get_type(ctx), 0) {
                    worklist.push(res);
                }
            }
        }
    }
    false
}

fn value_may_have_json_residue(ctx: &Context, root: Value) -> bool {
    let mut worklist = vec![root];
    let mut visited = std::collections::HashSet::new();
    while let Some(val) = worklist.pop() {
        if !visited.insert(val) {
            continue;
        }
        if !type_may_contain_record(ctx, val.get_type(ctx), 0) {
            continue;
        }
        let Some(def_op) = val.defining_op() else {
            return true;
        };
        if let Some(intr) = Operation::get_op::<IntrinsicOp>(def_op, ctx)
            && matches!(
                intr.intrinsic(ctx),
                Some(Intrinsic::JsonParse | Intrinsic::JsonResidue | Intrinsic::JsonRetain)
            )
        {
            return true;
        }
        if Operation::get_op::<CallOp>(def_op, ctx).is_some() {
            return true;
        }
        for use_site in val.uses(ctx) {
            if let Some(intr) = Operation::get_op::<IntrinsicOp>(use_site.user_op(), ctx)
                && matches!(
                    intr.intrinsic(ctx),
                    Some(Intrinsic::JsonResidue | Intrinsic::JsonRetain)
                )
            {
                return true;
            }
        }
        for operand in def_op.deref(ctx).operands() {
            if type_may_contain_record(ctx, operand.get_type(ctx), 0) {
                worklist.push(operand);
            }
        }
    }
    false
}

fn schemas_drop_fields(
    ctx: &Context,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
    depth: usize,
) -> bool {
    if depth > 16 {
        return true;
    }
    let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
    let t_inner = optional_value(ctx, target_ty).unwrap_or(target_ty);
    if s_inner == t_inner {
        return false;
    }
    if let (Some(s_rec), Some(t_rec)) = (
        s_inner.deref(ctx).downcast_ref::<RecordType>(),
        t_inner.deref(ctx).downcast_ref::<RecordType>(),
    ) {
        let (Ok(s_fields), Ok(t_fields)) = (s_rec.fields(), t_rec.fields()) else {
            return true;
        };
        if s_rec.is_tuple() {
            return s_fields
                .iter()
                .zip(t_fields.iter())
                .any(|((_, se), (_, te))| schemas_drop_fields(ctx, *se, *te, depth + 1));
        }
        if s_fields
            .iter()
            .any(|(s_name, _)| !t_fields.iter().any(|(t_name, _)| t_name == s_name))
        {
            return true;
        }
        return t_fields.iter().any(|(t_name, t_fty)| {
            s_fields
                .iter()
                .find(|(s_name, _)| s_name == t_name)
                .is_some_and(|(_, s_fty)| schemas_drop_fields(ctx, *s_fty, *t_fty, depth + 1))
        });
    }
    if let (Some(s_arr), Some(t_arr)) = (
        s_inner.deref(ctx).downcast_ref::<ArrayType>(),
        t_inner.deref(ctx).downcast_ref::<ArrayType>(),
    ) {
        return schemas_drop_fields(ctx, s_arr.element(), t_arr.element(), depth + 1);
    }
    false
}

fn schemas_compatible(
    ctx: &Context,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
    depth: usize,
) -> bool {
    if depth > 16 {
        return false;
    }

    let s_abs = absence(ctx, source_ty);
    let t_abs = absence(ctx, target_ty);

    if s_abs == Some(Absence::Both) || t_abs == Some(Absence::Both) {
        return false;
    }

    if t_abs == Some(Absence::Undefined) {
        let t_inner = optional_value(ctx, target_ty).unwrap_or(target_ty);
        if s_abs == Some(Absence::Undefined) {
            let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
            return schemas_compatible(ctx, s_inner, t_inner, depth + 1);
        }
        return schemas_compatible(ctx, source_ty, t_inner, depth + 1);
    }

    if s_abs == Some(Absence::Undefined) {
        let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
        return schemas_compatible(ctx, s_inner, target_ty, depth + 1);
    }

    if s_abs == Some(Absence::Null) && t_abs != Some(Absence::Null) {
        return false;
    }

    let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
    let t_inner = optional_value(ctx, target_ty).unwrap_or(target_ty);

    if absence(ctx, s_inner).is_some() || absence(ctx, t_inner).is_some() {
        return false;
    }

    if s_inner == t_inner {
        if let Some(rec) = s_inner.deref(ctx).downcast_ref::<RecordType>() {
            let Ok(fields) = rec.fields() else {
                return false;
            };
            return fields
                .iter()
                .all(|(_, fty)| schemas_compatible(ctx, *fty, *fty, depth + 1));
        }
        if let Some(arr) = s_inner.deref(ctx).downcast_ref::<ArrayType>() {
            return schemas_compatible(ctx, arr.element(), arr.element(), depth + 1);
        }
        if let Some(var) = s_inner.deref(ctx).downcast_ref::<VariantType>() {
            if !variant_is_unambiguous(ctx, var) {
                return false;
            }
            return var
                .cases()
                .iter()
                .all(|(_, payload)| payload.is_none_or(|pty| schemas_compatible(ctx, pty, pty, depth + 1)));
        }
        return is_supported_scalar(ctx, s_inner);
    }

    if let (Some(s_rec), Some(t_rec)) = (
        s_inner.deref(ctx).downcast_ref::<RecordType>(),
        t_inner.deref(ctx).downcast_ref::<RecordType>(),
    ) {
        if s_rec.is_tuple() != t_rec.is_tuple() {
            return false;
        }
        let (Ok(s_fields), Ok(t_fields)) = (s_rec.fields(), t_rec.fields()) else {
            return false;
        };
        if s_rec.is_tuple() {
            if s_fields.len() != t_fields.len() {
                return false;
            }
            return s_fields
                .iter()
                .zip(t_fields.iter())
                .all(|((_, se), (_, te))| schemas_compatible(ctx, *se, *te, depth + 1));
        }
        if !t_fields.iter().all(|(t_name, t_fty)| {
            if let Some((_, s_fty)) = s_fields.iter().find(|(s_name, _)| s_name == t_name) {
                schemas_compatible(ctx, *s_fty, *t_fty, depth + 1)
            } else {
                absence(ctx, *t_fty) == Some(Absence::Undefined)
                    && schemas_compatible(ctx, *t_fty, *t_fty, depth + 1)
            }
        }) {
            return false;
        }
        return s_fields.iter().all(|(s_name, s_fty)| {
            if t_fields.iter().any(|(t_name, _)| t_name == s_name) {
                true
            } else {
                schemas_compatible(ctx, *s_fty, *s_fty, depth + 1)
            }
        });
    }

    if let (Some(s_arr), Some(t_arr)) = (
        s_inner.deref(ctx).downcast_ref::<ArrayType>(),
        t_inner.deref(ctx).downcast_ref::<ArrayType>(),
    ) {
        return schemas_compatible(ctx, s_arr.element(), t_arr.element(), depth + 1);
    }

    false
}

fn is_supported_scalar(ctx: &Context, ty: TypeHandle) -> bool {
    let obj = ty.deref(ctx);
    obj.is::<IntegerType>() || obj.is::<FP64Type>() || obj.is::<StringType>()
}

fn variant_is_unambiguous(ctx: &Context, var: &VariantType) -> bool {
    for (i, (_, p1)) in var.cases().iter().enumerate() {
        for (_, p2) in var.cases().iter().skip(i + 1) {
            match (p1, p2) {
                (None, None) => {}
                (Some(t1), Some(t2)) => {
                    if *t1 == *t2 {
                        return false;
                    }
                    let o1 = t1.deref(ctx);
                    let o2 = t2.deref(ctx);
                    if (o1.is::<RecordType>() && o2.is::<RecordType>())
                        || (o1.is::<ArrayType>() && o2.is::<ArrayType>())
                    {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    true
}

fn rewrite_site(ctx: &mut Context, site: &ParseStringifySite, helper_sym: Identifier) {
    let call = CallOp::new(
        ctx,
        helper_sym,
        vec![site.source_value],
        vec![site.target_ty],
    );
    call.get_operation().insert_before(ctx, site.consumer_op);
    let new_res = call.get_operation().deref(ctx).get_result(0);
    let old_res = site.consumer_op.deref(ctx).get_result(0);
    old_res.replace_some_uses_with(ctx, |_, _| true, &new_res);

    // Keep any CaughtOp immediately following `site.consumer_op` intact so it now
    // checks whether `call` (perry_json_copy_*) raised an exception inside a try block.
    Operation::erase(site.consumer_op, ctx);
    if let Some(parse_op) = site.intermediate_parse_op {
        erase_op_and_caught(ctx, parse_op);
    }
    if let Some(unwrap_op) = site.unwrap_op {
        erase_op_and_caught(ctx, unwrap_op);
    }
    erase_op_and_caught(ctx, site.stringify_op);
}

fn erase_op_and_caught(ctx: &mut Context, op: Ptr<Operation>) {
    let next = op.deref(ctx).get_next();
    if let Some(next) = next
        && Operation::get_op::<CaughtOp>(next, ctx).is_some()
    {
        let zero = const_i32(ctx, 0);
        zero.get_operation().insert_before(ctx, next);
        let zero_val = zero.get_result(ctx);
        let caught_res = next.deref(ctx).get_result(0);
        caught_res.replace_some_uses_with(ctx, |_, _| true, &zero_val);
        Operation::erase(next, ctx);
    }
    Operation::erase(op, ctx);
}

fn append_op(ctx: &mut Context, block: Ptr<BasicBlock>, op: &dyn Op) {
    op.get_operation().insert_at_back(block, ctx);
}

fn build_json_copy_helper(
    ctx: &mut Context,
    module: ModuleOp,
    sym: Identifier,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
) -> Result<()> {
    let fn_ty = FunctionType::get(ctx, vec![source_ty], vec![target_ty]);
    let func = FuncOp::new(ctx, sym, fn_ty);
    module.append_operation(ctx, func.get_operation(), 0);

    let entry = func.get_entry_block(ctx);
    let arg = entry.deref(ctx).get_argument(0);
    let (end_block, norm_val) =
        emit_normalize_value(ctx, entry, arg, source_ty, target_ty, CopyPosition::Root)?;
    let ret = ReturnOp::new(ctx, vec![norm_val]);
    append_op(ctx, end_block, &ret);
    Ok(())
}

fn emit_normalize_value(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    source: Value,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
    pos: CopyPosition,
) -> Result<(Ptr<BasicBlock>, Value)> {
    let s_abs = absence(ctx, source_ty);
    let t_abs = absence(ctx, target_ty);

    // Case 1: Target outer wrapper is `OptionalType(t_inner, Absence::Undefined)`.
    if t_abs == Some(Absence::Undefined) {
        let t_inner = optional_value(ctx, target_ty).unwrap_or(target_ty);
        if s_abs == Some(Absence::Undefined) {
            let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
            let absent_block = BasicBlock::new(ctx, None, vec![]);
            let present_block = BasicBlock::new(ctx, None, vec![]);
            let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
            absent_block.insert_after(ctx, block);
            present_block.insert_after(ctx, absent_block);
            merge_block.insert_after(ctx, present_block);

            let is_undef = IsUndefinedOp::new(ctx, source);
            append_op(ctx, block, &is_undef);
            let cond = is_undef.get_result(ctx);
            let br = CondBrOp::new(ctx, cond, absent_block, vec![], present_block, vec![]);
            append_op(ctx, block, &br);

            if pos == CopyPosition::RecordField {
                let undef = UndefinedOp::with_type(ctx, target_ty);
                append_op(ctx, absent_block, &undef);
                let br_absent = BrOp::new(ctx, merge_block, vec![undef.get_result(ctx)]);
                append_op(ctx, absent_block, &br_absent);
            } else {
                emit_throw_type_error(
                    ctx,
                    absent_block,
                    "undefined value is not valid in JSON at this position",
                );
            }

            let unwrap = UnwrapOp::new(ctx, source)?;
            append_op(ctx, present_block, &unwrap);
            let s_unwrapped = unwrap.get_result(ctx);
            let (present_end, inner_norm) =
                emit_normalize_value(ctx, present_block, s_unwrapped, s_inner, t_inner, pos)?;
            let wrapped = WrapOp::with_type(ctx, inner_norm, target_ty);
            append_op(ctx, present_end, &wrapped);
            let br_present = BrOp::new(ctx, merge_block, vec![wrapped.get_result(ctx)]);
            append_op(ctx, present_end, &br_present);

            return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
        }

        let (end_block, inner_norm) =
            emit_normalize_value(ctx, block, source, source_ty, t_inner, pos)?;
        let wrapped = WrapOp::with_type(ctx, inner_norm, target_ty);
        append_op(ctx, end_block, &wrapped);
        return Ok((end_block, wrapped.get_result(ctx)));
    }

    // Case 2: Source has an outer optional/nullable wrapper (`Absence::Undefined` or `Absence::Null`).
    if let Some(s_kind) = s_abs {
        let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
        let absent_block = BasicBlock::new(ctx, None, vec![]);
        let present_block = BasicBlock::new(ctx, None, vec![]);
        let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
        absent_block.insert_after(ctx, block);
        present_block.insert_after(ctx, absent_block);
        merge_block.insert_after(ctx, present_block);

        let is_absent = match s_kind {
            Absence::Undefined | Absence::Both => {
                let op = IsUndefinedOp::new(ctx, source);
                append_op(ctx, block, &op);
                op.get_result(ctx)
            }
            Absence::Null => {
                let op = IsNullOp::new(ctx, source);
                append_op(ctx, block, &op);
                op.get_result(ctx)
            }
        };
        let br = CondBrOp::new(ctx, is_absent, absent_block, vec![], present_block, vec![]);
        append_op(ctx, block, &br);

        let absent_becomes_null = match s_kind {
            Absence::Null => t_abs == Some(Absence::Null),
            Absence::Undefined => pos == CopyPosition::Element && t_abs == Some(Absence::Null),
            Absence::Both => false,
        };
        if absent_becomes_null {
            let null_op = NullOp::new(ctx, target_ty);
            append_op(ctx, absent_block, &null_op);
            let br_absent = BrOp::new(ctx, merge_block, vec![null_op.get_result(ctx)]);
            append_op(ctx, absent_block, &br_absent);
        } else {
            emit_throw_type_error(ctx, absent_block, "unexpected absent value in JSON copy");
        }

        let unwrap = UnwrapOp::new(ctx, source)?;
        append_op(ctx, present_block, &unwrap);
        let s_unwrapped = unwrap.get_result(ctx);
        // Recurse with `target_ty` intact so that if `target_ty` has `Absence::Null` and
        // `s_unwrapped` is `NaN` or `Infinity`, the `FloatType` handler can still produce `null`.
        let (present_end, present_norm) =
            emit_normalize_value(ctx, present_block, s_unwrapped, s_inner, target_ty, pos)?;
        let br_present = BrOp::new(ctx, merge_block, vec![present_norm]);
        append_op(ctx, present_end, &br_present);

        return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
    }

    let s_inner = source_ty;
    let t_inner = optional_value(ctx, target_ty).unwrap_or(target_ty);

    // Scalar: FP64Type (`f64`)
    if s_inner.deref(ctx).is::<FP64Type>() && t_inner.deref(ctx).is::<FP64Type>() {
        let is_finite = IntrinsicOp::new(ctx, Intrinsic::NumberIsFinite, vec![source]);
        append_op(ctx, block, &is_finite);
        let is_finite_val = is_finite.get_operation().deref(ctx).get_result(0);

        let finite_block = BasicBlock::new(ctx, None, vec![]);
        let non_finite_block = BasicBlock::new(ctx, None, vec![]);
        let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
        finite_block.insert_after(ctx, block);
        non_finite_block.insert_after(ctx, finite_block);
        merge_block.insert_after(ctx, non_finite_block);

        let br = CondBrOp::new(
            ctx,
            is_finite_val,
            finite_block,
            vec![],
            non_finite_block,
            vec![],
        );
        append_op(ctx, block, &br);

        // Canonicalize `-0.0` to `+0.0` via `source + 0.0`.
        let pos_zero = const_f64(ctx, 0.0);
        append_op(ctx, finite_block, &pos_zero);
        let add_zero = BinaryOp::new(ctx, BinaryKind::Add, source, pos_zero.get_result(ctx));
        append_op(ctx, finite_block, &add_zero);
        let normalized = add_zero.get_result(ctx);
        let finite_out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, normalized, target_ty);
            append_op(ctx, finite_block, &wrap);
            wrap.get_result(ctx)
        } else {
            normalized
        };
        let br_finite = BrOp::new(ctx, merge_block, vec![finite_out]);
        append_op(ctx, finite_block, &br_finite);

        if t_abs == Some(Absence::Null) {
            let null_op = NullOp::new(ctx, target_ty);
            append_op(ctx, non_finite_block, &null_op);
            let br_nf = BrOp::new(ctx, merge_block, vec![null_op.get_result(ctx)]);
            append_op(ctx, non_finite_block, &br_nf);
        } else {
            emit_throw_type_error(
                ctx,
                non_finite_block,
                "non-finite number is not valid for non-nullable target schema",
            );
        }

        return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
    }

    // Scalar: Integer or String
    if is_supported_scalar(ctx, s_inner) && s_inner == t_inner {
        let out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, source, target_ty);
            append_op(ctx, block, &wrap);
            wrap.get_result(ctx)
        } else {
            source
        };
        return Ok((block, out));
    }

    // RecordType (both records and tuples): allocate a fresh Record of `t_inner` and normalize/project fields
    let rec_fields = {
        let s_obj = s_inner.deref(ctx);
        let t_obj = t_inner.deref(ctx);
        if let (Some(s_rec), Some(t_rec)) = (
            s_obj.downcast_ref::<RecordType>(),
            t_obj.downcast_ref::<RecordType>(),
        ) {
            let child_pos = if t_rec.is_tuple() {
                CopyPosition::Element
            } else {
                CopyPosition::RecordField
            };
            Some((s_rec.fields()?.to_vec(), t_rec.fields()?.to_vec(), child_pos))
        } else {
            None
        }
    };
    if let Some((s_fields, t_fields, child_pos)) = rec_fields {
        let mut cur_block = block;
        let mut field_vals = Vec::with_capacity(t_fields.len());

        for (t_name, t_fty) in &t_fields {
            if let Some((_, s_fty)) = s_fields.iter().find(|(s_name, _)| s_name == t_name) {
                let get = FieldGetOp::new(ctx, source, t_name.clone())?;
                append_op(ctx, cur_block, &get);
                let raw_val = get.get_result(ctx);
                let (next_block, norm_val) =
                    emit_normalize_value(ctx, cur_block, raw_val, *s_fty, *t_fty, child_pos)?;
                cur_block = next_block;
                field_vals.push(norm_val);
            } else {
                let undef = UndefinedOp::with_type(ctx, *t_fty);
                append_op(ctx, cur_block, &undef);
                field_vals.push(undef.get_result(ctx));
            }
        }

        let t_rec_handle =
            TypedHandle::<RecordType>::from_handle(t_inner, ctx).expect("RecordType handle");
        let alloc = AllocOp::new(ctx, t_rec_handle, field_vals);
        append_op(ctx, cur_block, &alloc);
        let rec_val = alloc.get_result(ctx);
        let out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, rec_val, target_ty);
            append_op(ctx, cur_block, &wrap);
            wrap.get_result(ctx)
        } else {
            rec_val
        };
        return Ok((cur_block, out));
    }

    // ArrayType: allocate a fresh array and normalize each element in a loop
    let arr_elems = {
        let s_obj = s_inner.deref(ctx);
        let t_obj = t_inner.deref(ctx);
        if let (Some(s_arr), Some(t_arr)) = (
            s_obj.downcast_ref::<ArrayType>(),
            t_obj.downcast_ref::<ArrayType>(),
        ) {
            Some((s_arr.element(), t_arr.element()))
        } else {
            None
        }
    };
    if let Some((s_elem_ty, t_elem_ty)) = arr_elems {
        let len_op = ArrayLenOp::new(ctx, source);
        append_op(ctx, block, &len_op);
        let len_val = len_op.get_result(ctx);

        let new_arr = ArrayNewOp::new(ctx, t_elem_ty, vec![]);
        append_op(ctx, block, &new_arr);
        let init_arr = new_arr.get_result(ctx);

        let zero = const_i32(ctx, 0);
        append_op(ctx, block, &zero);
        let idx0 = zero.get_result(ctx);
        let i32_ty = idx0.get_type(ctx);

        let header_block = BasicBlock::new(ctx, None, vec![i32_ty]);
        let body_block = BasicBlock::new(ctx, None, vec![]);
        let exit_block = BasicBlock::new(ctx, None, vec![]);
        header_block.insert_after(ctx, block);
        body_block.insert_after(ctx, header_block);
        exit_block.insert_after(ctx, body_block);

        let br_init = BrOp::new(ctx, header_block, vec![idx0]);
        append_op(ctx, block, &br_init);

        let idx_arg = header_block.deref(ctx).get_argument(0);
        let cmp = CompareOp::new(ctx, CompareKind::Lt, idx_arg, len_val);
        append_op(ctx, header_block, &cmp);
        let br_cond = CondBrOp::new(
            ctx,
            cmp.get_result(ctx),
            body_block,
            vec![],
            exit_block,
            vec![],
        );
        append_op(ctx, header_block, &br_cond);

        let at_op = ArrayAtOp::new(ctx, source, idx_arg)?;
        append_op(ctx, body_block, &at_op);
        let elem_val = at_op.get_result(ctx);
        let (body_end, norm_elem) = emit_normalize_value(
            ctx,
            body_block,
            elem_val,
            s_elem_ty,
            t_elem_ty,
            CopyPosition::Element,
        )?;
        let push_op = ArrayPushOp::new(ctx, init_arr, norm_elem);
        append_op(ctx, body_end, &push_op);
        let one = const_i32(ctx, 1);
        append_op(ctx, body_end, &one);
        let next_idx = BinaryOp::new(ctx, BinaryKind::Add, idx_arg, one.get_result(ctx));
        append_op(ctx, body_end, &next_idx);
        let br_back = BrOp::new(ctx, header_block, vec![next_idx.get_result(ctx)]);
        append_op(ctx, body_end, &br_back);

        let out = if t_abs.is_some() {
            let wrap = WrapOp::with_type(ctx, init_arr, target_ty);
            append_op(ctx, exit_block, &wrap);
            wrap.get_result(ctx)
        } else {
            init_arr
        };
        return Ok((exit_block, out));
    }

    // VariantType: dispatch on variant tag, normalize payload if present, reconstruct variant
    let var_cases = if s_inner == t_inner {
        s_inner
            .deref(ctx)
            .downcast_ref::<VariantType>()
            .map(|var| var.cases().to_vec())
    } else {
        None
    };
    if let Some(cases) = var_cases {
        let tag_op = VariantTagOp::new(ctx, source);
        append_op(ctx, block, &tag_op);
        let tag_val = tag_op.get_result(ctx);

        let merge_block = BasicBlock::new(ctx, None, vec![target_ty]);
        merge_block.insert_after(ctx, block);

        let mut cur_check_block = block;
        for (idx, (case_name, payload_ty)) in cases.iter().enumerate() {
            let is_last = idx + 1 == cases.len();
            let case_block = BasicBlock::new(ctx, None, vec![]);
            case_block.insert_before(ctx, merge_block);

            if is_last {
                let br = BrOp::new(ctx, case_block, vec![]);
                append_op(ctx, cur_check_block, &br);
            } else {
                let next_check_block = BasicBlock::new(ctx, None, vec![]);
                next_check_block.insert_before(ctx, merge_block);
                let tag_const = StringConstOp::new(ctx, case_name);
                append_op(ctx, cur_check_block, &tag_const);
                let cmp =
                    CompareOp::new(ctx, CompareKind::Eq, tag_val, tag_const.get_result(ctx));
                append_op(ctx, cur_check_block, &cmp);
                let br = CondBrOp::new(
                    ctx,
                    cmp.get_result(ctx),
                    case_block,
                    vec![],
                    next_check_block,
                    vec![],
                );
                append_op(ctx, cur_check_block, &br);
                cur_check_block = next_check_block;
            }

            let (case_end, new_var_val) = if let Some(pty) = payload_ty {
                let payload_op = VariantPayloadOp::new(ctx, source, *pty);
                append_op(ctx, case_block, &payload_op);
                let raw_payload = payload_op.get_result(ctx);
                let (norm_end, norm_payload) = emit_normalize_value(
                    ctx,
                    case_block,
                    raw_payload,
                    *pty,
                    *pty,
                    CopyPosition::RecordField,
                )?;
                let vnew = VariantNewOp::new(ctx, t_inner, case_name, Some(norm_payload));
                append_op(ctx, norm_end, &vnew);
                (norm_end, vnew.get_result(ctx))
            } else {
                let vnew = VariantNewOp::new(ctx, t_inner, case_name, None);
                append_op(ctx, case_block, &vnew);
                (case_block, vnew.get_result(ctx))
            };

            let out = if t_abs.is_some() {
                let wrap = WrapOp::with_type(ctx, new_var_val, target_ty);
                append_op(ctx, case_end, &wrap);
                wrap.get_result(ctx)
            } else {
                new_var_val
            };
            let br_merge = BrOp::new(ctx, merge_block, vec![out]);
            append_op(ctx, case_end, &br_merge);
        }

        return Ok((merge_block, merge_block.deref(ctx).get_argument(0)));
    }

    Ok((block, source))
}

fn emit_throw_type_error(ctx: &mut Context, block: Ptr<BasicBlock>, message: &str) {
    let msg = StringConstOp::new(ctx, message);
    append_op(ctx, block, &msg);
    let err = ErrorNewOp::new(ctx, ErrorKind::TypeError, msg.get_result(ctx));
    append_op(ctx, block, &err);
    let status = RaiseOp::new(ctx, err.get_result(ctx));
    append_op(ctx, block, &status);
    let rethrow = RethrowOp::new(ctx, status.get_result(ctx));
    append_op(ctx, block, &rethrow);
}

fn const_i32(ctx: &mut Context, val: i32) -> ConstantOp {
    let ty = IntegerType::get(ctx, 32, Signedness::Signless);
    let attr = IntegerAttr::new(ty, APInt::from_u64(val as u32 as u64, bw(32)));
    ConstantOp::new(ctx, Box::new(attr))
}

fn const_f64(ctx: &mut Context, val: f64) -> ConstantOp {
    let attr = FPDoubleAttr::from(val);
    ConstantOp::new(ctx, Box::new(attr))
}
