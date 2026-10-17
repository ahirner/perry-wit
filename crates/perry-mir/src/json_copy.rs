mod emit;

use std::collections::{HashMap, HashSet};

use pliron::builtin::op_interfaces::{
    OneRegionInterface, SingleBlockRegionInterface, SymbolOpInterface,
};
use pliron::builtin::ops::{FuncOp, ModuleOp};
use pliron::builtin::types::{FP64Type, IntegerType};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::irbuild::{
    IRStatus,
    inserter::{Inserter, OpInsertionPoint},
    listener::DummyListener,
    rewriter::{IRRewriter, Rewriter},
};
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
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let Some(module) = Operation::get_op::<ModuleOp>(root, ctx) else {
            return Ok(PassResult::default());
        };
        let facts = analyses.get_analysis::<crate::facts::ProgramFacts>(root, ctx)?;

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
        let funcs_by_name: HashMap<Identifier, FuncOp> =
            funcs.iter().map(|&f| (f.get_symbol_name(ctx), f)).collect();
        let mut used_names: HashSet<Identifier> = funcs_by_name.keys().cloned().collect();

        for func in funcs {
            let sites = find_parse_stringify_sites(func, ctx, &funcs_by_name);
            for site in sites {
                let source_ty = site.source_value.get_type(ctx);
                let target_ty = site.target_ty;
                if can_rewrite_parse_stringify(
                    ctx,
                    &facts,
                    func,
                    &funcs_by_name,
                    &site,
                    source_ty,
                    target_ty,
                ) {
                    let helper_sym = if let Some(sym) =
                        helper_cache.get(&(source_ty, target_ty)).cloned()
                    {
                        sym
                    } else {
                        let sym = loop {
                            let candidate: Identifier = format!("perry_json_copy_{next_helper_id}")
                                .try_into()
                                .unwrap_or_else(|_| pliron::ident!("perry_json_copy"));
                            next_helper_id += 1;
                            if used_names.insert(candidate.clone()) {
                                break candidate;
                            }
                        };
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
            let mut stats = crate::pipeline::optimization_statistics(root, ctx);
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

fn find_parse_stringify_sites(
    func: FuncOp,
    ctx: &Context,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
) -> Vec<ParseStringifySite> {
    let mut sites = Vec::new();
    let region = func.get_region(ctx);

    for block in region.deref(ctx).iter(ctx) {
        for op in block.deref(ctx).iter(ctx) {
            if op.deref(ctx).get_num_operands() != 1 || op.deref(ctx).get_num_results() != 1 {
                continue;
            }
            let arg = op.deref(ctx).get_operand(0);
            let target_ty = op.deref(ctx).get_result(0).get_type(ctx);

            if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
                let decoder_kind = funcs_by_name
                    .get(&call.callee(ctx))
                    .and_then(|callee| crate::calls::json_decoder_kind(ctx, callee));
                match decoder_kind {
                    Some(crate::attrs::JsonDecoderKind::Parse) => {
                        if let Some((stringify_op, unwrap_op, source_value)) =
                            match_unwrapped_stringify(ctx, arg)
                        {
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
                    Some(crate::attrs::JsonDecoderKind::Value) => {
                        if let Some((parse_op, stringify_op, unwrap_op, source_value)) =
                            match_json_parse_of_stringify(ctx, arg)
                        {
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
                    _ => {}
                }
            } else if let Some(intrinsic_op) = Operation::get_op::<IntrinsicOp>(op, ctx) {
                match intrinsic_op.intrinsic(ctx) {
                    Some(
                        Intrinsic::JsonRequireNumber
                        | Intrinsic::JsonToNumber
                        | Intrinsic::JsonToI64
                        | Intrinsic::JsonToU64
                        | Intrinsic::JsonToBool
                        | Intrinsic::JsonToString,
                    ) => {
                        if let Some((parse_op, stringify_op, unwrap_op, source_value)) =
                            match_json_parse_of_stringify(ctx, arg)
                        {
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
                    Some(Intrinsic::JsonParse) => {
                        let parse_res = op.deref(ctx).get_result(0);
                        if !is_consumed_by_value_decoder(ctx, parse_res, funcs_by_name)
                            && let Some((stringify_op, unwrap_op, source_value)) =
                                match_unwrapped_stringify(ctx, arg)
                        {
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
                    _ => {}
                }
            }
        }
    }

    sites
}

fn is_consumed_by_value_decoder(
    ctx: &Context,
    parse_res: Value,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
) -> bool {
    parse_res.uses(ctx).iter().any(|use_site| {
        let user = use_site.user_op();
        if user.deref(ctx).get_num_operands() != 1 || user.deref(ctx).get_num_results() != 1 {
            return false;
        }
        if let Some(call) = Operation::get_op::<CallOp>(user, ctx) {
            return funcs_by_name
                .get(&call.callee(ctx))
                .and_then(|callee| crate::calls::json_decoder_kind(ctx, callee))
                == Some(crate::attrs::JsonDecoderKind::Value);
        }
        if let Some(intr) = Operation::get_op::<IntrinsicOp>(user, ctx) {
            return matches!(
                intr.intrinsic(ctx),
                Some(
                    Intrinsic::JsonRequireNumber
                        | Intrinsic::JsonToNumber
                        | Intrinsic::JsonToI64
                        | Intrinsic::JsonToU64
                        | Intrinsic::JsonToBool
                        | Intrinsic::JsonToString
                )
            );
        }
        false
    })
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
    if parse_intr.intrinsic(ctx) != Some(Intrinsic::JsonParse)
        || parse_op.deref(ctx).get_num_operands() != 1
    {
        return None;
    }
    let (stringify_op, unwrap_op, source_value) =
        match_unwrapped_stringify(ctx, parse_op.deref(ctx).get_operand(0))?;
    Some((parse_op, stringify_op, unwrap_op, source_value))
}

fn match_unwrapped_stringify(
    ctx: &Context,
    text_val: Value,
) -> Option<(Ptr<Operation>, Option<Ptr<Operation>>, Value)> {
    let def_op = text_val.defining_op()?;
    if Operation::get_op::<JsonStringifyOp>(def_op, ctx).is_some() {
        if def_op.deref(ctx).get_num_operands() != 1 {
            return None;
        }
        return Some((def_op, None, def_op.deref(ctx).get_operand(0)));
    }
    if Operation::get_op::<UnwrapOp>(def_op, ctx).is_some() {
        if def_op.deref(ctx).get_num_operands() != 1 {
            return None;
        }
        let stringify_op = def_op.deref(ctx).get_operand(0).defining_op()?;
        Operation::get_op::<JsonStringifyOp>(stringify_op, ctx)?;
        if stringify_op.deref(ctx).get_num_operands() != 1 {
            return None;
        }
        return Some((
            stringify_op,
            Some(def_op),
            stringify_op.deref(ctx).get_operand(0),
        ));
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
    facts: &crate::facts::ProgramFacts,
    func: FuncOp,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
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
    let reaches_stringify = value_reaches_stringify(ctx, consumer_res, funcs_by_name);
    if reaches_stringify && schemas_drop_fields(ctx, source_ty, target_ty, 0) {
        return false;
    }
    if (reaches_stringify || schemas_add_optional_fields(ctx, source_ty, target_ty, 0))
        && facts.may_have_json_residue(ctx, &func, site.source_value)
    {
        return false;
    }

    if !schemas_compatible(ctx, source_ty, target_ty, 0) {
        return false;
    }

    !facts.may_prevent_json_copy_between(
        ctx,
        &func,
        site.source_value,
        site.stringify_op,
        (site.unwrap_op, site.intermediate_parse_op),
        site.consumer_op,
    )
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

fn value_reaches_stringify(
    ctx: &Context,
    root: Value,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
) -> bool {
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
            if let Some(intr) = Operation::get_op::<IntrinsicOp>(user, ctx) {
                match intr.intrinsic(ctx) {
                    Some(Intrinsic::JsonResidue | Intrinsic::JsonRetain) => return true,
                    Some(Intrinsic::JsonSpread) => {
                        let op_ref = user.deref(ctx);
                        if op_ref.get_num_operands() >= 2 && val == op_ref.get_operand(1) {
                            worklist.push(op_ref.get_operand(0));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(call) = Operation::get_op::<CallOp>(user, ctx)
                && funcs_by_name
                    .get(&call.callee(ctx))
                    .and_then(|callee| crate::calls::json_decoder_kind(ctx, callee))
                    .is_none()
            {
                return true;
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

fn schemas_add_optional_fields(
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
                .any(|((_, se), (_, te))| schemas_add_optional_fields(ctx, *se, *te, depth + 1));
        }
        if t_fields
            .iter()
            .any(|(t_name, _)| !s_fields.iter().any(|(s_name, _)| s_name == t_name))
        {
            return true;
        }
        return t_fields.iter().any(|(t_name, t_fty)| {
            s_fields
                .iter()
                .find(|(s_name, _)| s_name == t_name)
                .is_some_and(|(_, s_fty)| {
                    schemas_add_optional_fields(ctx, *s_fty, *t_fty, depth + 1)
                })
        });
    }
    if let (Some(s_arr), Some(t_arr)) = (
        s_inner.deref(ctx).downcast_ref::<ArrayType>(),
        t_inner.deref(ctx).downcast_ref::<ArrayType>(),
    ) {
        return schemas_add_optional_fields(ctx, s_arr.element(), t_arr.element(), depth + 1);
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
    let mut rewriter = IRRewriter::<DummyListener>::default();
    rewriter.set_insertion_point(OpInsertionPoint::BeforeOperation(site.consumer_op));
    let call = CallOp::new(
        ctx,
        helper_sym,
        vec![site.source_value],
        vec![site.target_ty],
    );
    rewriter.insert_operation(ctx, call.get_operation());
    // Keep any CaughtOp immediately following `site.consumer_op` intact so it now
    // checks whether `call` (perry_json_copy_*) raised an exception inside a try block.
    rewriter.replace_operation(ctx, site.consumer_op, call.get_operation());
    if let Some(parse_op) = site.intermediate_parse_op {
        erase_op_and_caught(ctx, &mut rewriter, parse_op);
    }
    if let Some(unwrap_op) = site.unwrap_op {
        erase_op_and_caught(ctx, &mut rewriter, unwrap_op);
    }
    erase_op_and_caught(ctx, &mut rewriter, site.stringify_op);
}

fn erase_op_and_caught(
    ctx: &mut Context,
    rewriter: &mut IRRewriter<DummyListener>,
    op: Ptr<Operation>,
) {
    let next = op.deref(ctx).get_next();
    if let Some(next) = next
        && Operation::get_op::<CaughtOp>(next, ctx).is_some()
    {
        rewriter.set_insertion_point(OpInsertionPoint::BeforeOperation(next));
        let zero = const_i32(ctx, 0);
        rewriter.insert_operation(ctx, zero.get_operation());
        rewriter.replace_operation(ctx, next, zero.get_operation());
    }
    rewriter.erase_operation(ctx, op);
}
