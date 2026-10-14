mod emit;

use std::collections::HashMap;

use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::{
    OneRegionInterface, OneResultInterface, SingleBlockRegionInterface,
};
use pliron::builtin::ops::{FuncOp, ModuleOp};
use pliron::builtin::types::{FP64Type, IntegerType};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::irbuild::IRStatus;
use pliron::linked_list::{ContainsLinkedList, LinkedList};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::pass::{AnalysisManager, Pass, PassResult};
use pliron::result::Result;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;

use crate::attrs::Absence;
use crate::calls::CallOp;
use crate::control::{BrOp, CondBrOp};
use crate::exceptions::CaughtOp;
use crate::intrinsics::{Intrinsic, IntrinsicOp, JsonStringifyOp};
use crate::optional::{UnwrapOp, absence, optional_value};
use crate::types::{ArrayType, JsonType, RecordType, StringType, VariantType};

use self::emit::{build_json_copy_helper, const_i32};

/// MIR pass that recognizes `JSON.parse<T>(JSON.stringify(value))` and rewrites
/// provably compatible schemas into a schema-driven deep-copy/normalization helper
/// without constructing intermediate JSON text or a JSON cursor.
pub(crate) struct NormalizeJsonCopy;

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
                    let helper_sym =
                        if let Some(sym) = helper_cache.get(&(source_ty, target_ty)).cloned() {
                            sym
                        } else {
                            let sym: Identifier = format!("perry_json_copy_{next_helper_id}")
                                .try_into()
                                .unwrap_or_else(|_| pliron::ident!("perry_json_copy"));
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

type MatchedParseOfStringify = (
    Ptr<Operation>,
    Ptr<Operation>,
    Option<Ptr<Operation>>,
    Value,
);

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
            return var.cases().iter().all(|(_, payload)| {
                payload.is_none_or(|pty| schemas_compatible(ctx, pty, pty, depth + 1))
            });
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
