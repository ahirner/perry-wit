mod emit;

use std::collections::{HashMap, HashSet};

use pliron::builtin::op_interfaces::{SingleBlockRegionInterface, SymbolOpInterface};
use pliron::builtin::ops::{FuncOp, ModuleOp};
use pliron::builtin::types::{FP64Type, IntegerType};
use pliron::context::{Context, Ptr};
use pliron::identifier::{Identifier, Legaliser};
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

use crate::attrs::{Absence, JsonDecoderKind};
use crate::calls::CallOp;
use crate::control::{BrOp, CondBrOp};
use crate::exceptions::CaughtOp;
use crate::intrinsics::{Intrinsic, JsonStringifyOp, intrinsic_of};
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
                    let helper_sym =
                        if let Some(sym) = helper_cache.get(&(source_ty, target_ty)).cloned() {
                            sym
                        } else {
                            let sym = loop {
                                // Already a legal identifier, so legalising is the identity.
                                let candidate = Legaliser::default()
                                    .legalise(&format!("perry_json_copy_{next_helper_id}"));
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
    crate::aggregates::operations(ctx, &func)
        .into_iter()
        .filter_map(|op| parse_stringify_site(ctx, op, funcs_by_name))
        .collect()
}

/// The site `op` consumes, when it decodes `JSON.parse(JSON.stringify(value))`: a decoder of
/// text directly, or a value decoder of the parsed JSON.
fn parse_stringify_site(
    ctx: &Context,
    op: Ptr<Operation>,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
) -> Option<ParseStringifySite> {
    if !is_unary(ctx, op) {
        return None;
    }
    let arg = op.deref(ctx).get_operand(0);
    let (intermediate_parse_op, text) = if is_value_decoder(ctx, op, funcs_by_name) {
        let parse_op = arg.defining_op()?;
        let parses = intrinsic_of(ctx, parse_op) == Some(Intrinsic::JsonParse);
        (parses && parse_op.deref(ctx).get_num_operands() == 1)
            .then(|| (Some(parse_op), parse_op.deref(ctx).get_operand(0)))?
    } else if decoder_kind(ctx, op, funcs_by_name) == Some(JsonDecoderKind::Parse)
        || intrinsic_of(ctx, op) == Some(Intrinsic::JsonParse)
            // A parse a value decoder consumes is matched from that decoder instead.
            && !op.deref(ctx).get_result(0).uses(ctx).iter().any(|use_site| {
                let user = use_site.user_op();
                is_unary(ctx, user) && is_value_decoder(ctx, user, funcs_by_name)
            })
    {
        (None, arg)
    } else {
        return None;
    };
    let (stringify_op, unwrap_op, source_value) = match_unwrapped_stringify(ctx, text)?;
    Some(ParseStringifySite {
        stringify_op,
        unwrap_op,
        consumer_op: op,
        intermediate_parse_op,
        source_value,
        target_ty: op.deref(ctx).get_result(0).get_type(ctx),
    })
}

fn is_unary(ctx: &Context, op: Ptr<Operation>) -> bool {
    let operation = op.deref(ctx);
    operation.get_num_operands() == 1 && operation.get_num_results() == 1
}

/// The kind of the generated JSON decoder `op` calls, if it calls one.
fn decoder_kind(
    ctx: &Context,
    op: Ptr<Operation>,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
) -> Option<JsonDecoderKind> {
    let call = Operation::get_op::<CallOp>(op, ctx)?;
    crate::calls::json_decoder_kind(ctx, funcs_by_name.get(&call.callee(ctx))?)
}

/// Whether `op` decodes a parsed JSON value: a generated value decoder or a scalar conversion.
fn is_value_decoder(
    ctx: &Context,
    op: Ptr<Operation>,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
) -> bool {
    decoder_kind(ctx, op, funcs_by_name) == Some(JsonDecoderKind::Value)
        || matches!(
            intrinsic_of(ctx, op),
            Some(
                Intrinsic::JsonRequireNumber
                    | Intrinsic::JsonToNumber
                    | Intrinsic::JsonToI64
                    | Intrinsic::JsonToU64
                    | Intrinsic::JsonToBool
                    | Intrinsic::JsonToString
            )
        )
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
    if reaches_stringify && schemas_add_fields(ctx, target_ty, source_ty, &mut SchemaPairs::new()) {
        return false;
    }
    if (reaches_stringify || schemas_add_fields(ctx, source_ty, target_ty, &mut SchemaPairs::new()))
        && facts.may_have_json_residue(ctx, &func, site.source_value)
    {
        return false;
    }

    if !schemas_compatible(ctx, source_ty, target_ty, &mut SchemaPairs::new()) {
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

/// Whether a `ty` value may hold a record. `seen` holds the types already visited, so a
/// recursive type ends its cycle: revisiting adds no new answer to an `any`.
fn type_may_contain_record(ctx: &Context, ty: TypeHandle, seen: &mut HashSet<TypeHandle>) -> bool {
    if !seen.insert(ty) {
        return false;
    }
    let inner = optional_value(ctx, ty).unwrap_or(ty);
    if let Some(rec) = inner.deref(ctx).downcast_ref::<RecordType>() {
        if rec.is_tuple() {
            return rec
                .fields()
                .map(|fields| {
                    fields
                        .iter()
                        .any(|(_, elem)| type_may_contain_record(ctx, *elem, seen))
                })
                .unwrap_or(true);
        }
        return true;
    }
    if inner.deref(ctx).downcast_ref::<JsonType>().is_some() {
        return true;
    }
    if let Some(arr) = inner.deref(ctx).downcast_ref::<ArrayType>() {
        return type_may_contain_record(ctx, arr.element(), seen);
    }
    if let Some(var) = inner.deref(ctx).downcast_ref::<VariantType>() {
        return var.cases().iter().any(|(_, payload)| {
            payload.is_some_and(|payload| type_may_contain_record(ctx, payload, seen))
        });
    }
    false
}

fn value_reaches_stringify(
    ctx: &Context,
    root: Value,
    funcs_by_name: &HashMap<Identifier, FuncOp>,
) -> bool {
    let may_contain_record = |ty| type_may_contain_record(ctx, ty, &mut HashSet::new());
    if !may_contain_record(root.get_type(ctx)) {
        return false;
    }
    let mut worklist = vec![root];
    let mut visited = HashSet::new();
    while let Some(val) = worklist.pop() {
        if !visited.insert(val) {
            continue;
        }
        if !may_contain_record(val.get_type(ctx)) {
            continue;
        }
        for use_site in val.uses(ctx) {
            let user = use_site.user_op();
            if Operation::get_op::<JsonStringifyOp>(user, ctx).is_some() {
                return true;
            }
            match intrinsic_of(ctx, user) {
                Some(Intrinsic::JsonResidue | Intrinsic::JsonRetain) => return true,
                Some(Intrinsic::JsonSpread) => {
                    let op_ref = user.deref(ctx);
                    if op_ref.get_num_operands() >= 2 && val == op_ref.get_operand(1) {
                        worklist.push(op_ref.get_operand(0));
                    }
                }
                _ => {}
            }
            if Operation::get_op::<CallOp>(user, ctx).is_some()
                && decoder_kind(ctx, user, funcs_by_name).is_none()
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
                        if may_contain_record(arg.get_type(ctx)) {
                            worklist.push(arg);
                        }
                    }
                }
            }
            let num_res = user.deref(ctx).get_num_results();
            for idx in 0..num_res {
                let res = user.deref(ctx).get_result(idx);
                if may_contain_record(res.get_type(ctx)) {
                    worklist.push(res);
                }
            }
        }
    }
    false
}

/// Pairs of source and target types already visited by a schema comparison. Recursive schemas
/// revisit a pair: an `any` comparison answers `false` there and an `all` one `true`, since the
/// first visit decides the pair (coinductively for `all`).
type SchemaPairs = HashSet<(TypeHandle, TypeHandle)>;

/// Whether converting `source_ty` to `target_ty` adds a field the source lacks. Swapping the
/// types asks whether the conversion drops a source field; a tuple-ness mismatch, where the
/// two directions differ, fails [schemas_compatible] anyway.
fn schemas_add_fields(
    ctx: &Context,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
    seen: &mut SchemaPairs,
) -> bool {
    if !seen.insert((source_ty, target_ty)) {
        return false;
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
                .any(|((_, se), (_, te))| schemas_add_fields(ctx, *se, *te, seen));
        }
        // A missing field answers `true` whichever matched fields are compared first.
        return t_fields.iter().any(|(t_name, t_fty)| {
            s_fields
                .iter()
                .find(|(s_name, _)| s_name == t_name)
                .is_none_or(|(_, s_fty)| schemas_add_fields(ctx, *s_fty, *t_fty, seen))
        });
    }
    if let (Some(s_arr), Some(t_arr)) = (
        s_inner.deref(ctx).downcast_ref::<ArrayType>(),
        t_inner.deref(ctx).downcast_ref::<ArrayType>(),
    ) {
        return schemas_add_fields(ctx, s_arr.element(), t_arr.element(), seen);
    }
    false
}

/// Whether a typed copy from `source_ty` to `target_ty` matches a JSON round trip.
fn schemas_compatible(
    ctx: &Context,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
    seen: &mut SchemaPairs,
) -> bool {
    if !seen.insert((source_ty, target_ty)) {
        return true;
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
            return schemas_compatible(ctx, s_inner, t_inner, seen);
        }
        return schemas_compatible(ctx, source_ty, t_inner, seen);
    }

    if s_abs == Some(Absence::Undefined) {
        let s_inner = optional_value(ctx, source_ty).unwrap_or(source_ty);
        return schemas_compatible(ctx, s_inner, target_ty, seen);
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
                .all(|(_, fty)| schemas_compatible(ctx, *fty, *fty, seen));
        }
        if let Some(arr) = s_inner.deref(ctx).downcast_ref::<ArrayType>() {
            return schemas_compatible(ctx, arr.element(), arr.element(), seen);
        }
        if let Some(var) = s_inner.deref(ctx).downcast_ref::<VariantType>() {
            if !variant_is_unambiguous(ctx, var) {
                return false;
            }
            return var.cases().iter().all(|(_, payload)| {
                payload.is_none_or(|pty| schemas_compatible(ctx, pty, pty, seen))
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
                .all(|((_, se), (_, te))| schemas_compatible(ctx, *se, *te, seen));
        }
        if !t_fields.iter().all(|(t_name, t_fty)| {
            if let Some((_, s_fty)) = s_fields.iter().find(|(s_name, _)| s_name == t_name) {
                schemas_compatible(ctx, *s_fty, *t_fty, seen)
            } else {
                absence(ctx, *t_fty) == Some(Absence::Undefined)
                    && schemas_compatible(ctx, *t_fty, *t_fty, seen)
            }
        }) {
            return false;
        }
        return s_fields.iter().all(|(s_name, s_fty)| {
            if t_fields.iter().any(|(t_name, _)| t_name == s_name) {
                true
            } else {
                schemas_compatible(ctx, *s_fty, *s_fty, seen)
            }
        });
    }

    if let (Some(s_arr), Some(t_arr)) = (
        s_inner.deref(ctx).downcast_ref::<ArrayType>(),
        t_inner.deref(ctx).downcast_ref::<ArrayType>(),
    ) {
        return schemas_compatible(ctx, s_arr.element(), t_arr.element(), seen);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OptionalType;

    #[test]
    fn schema_field_changes_are_directional() -> Result<()> {
        let ctx = &mut Context::new();
        let number: TypeHandle = FP64Type::get(ctx).into();
        let wide: TypeHandle = RecordType::get(
            ctx,
            pliron::ident!("Wide"),
            Some(vec![("x".into(), number), ("y".into(), number)]),
        )?
        .into();
        let narrow: TypeHandle = RecordType::get(
            ctx,
            pliron::ident!("Narrow"),
            Some(vec![("x".into(), number)]),
        )?
        .into();
        let outer = |ctx: &mut Context, name, inner: TypeHandle| -> Result<TypeHandle> {
            let inner = OptionalType::get(ctx, ArrayType::get(ctx, inner).into()).into();
            Ok(RecordType::get(ctx, name, Some(vec![("items".into(), inner)]))?.into())
        };
        let wide_outer = outer(ctx, pliron::ident!("WideOuter"), wide)?;
        let narrow_outer = outer(ctx, pliron::ident!("NarrowOuter"), narrow)?;
        for (source, target, adds, drops) in [
            (wide, narrow, false, true),
            (narrow, wide, true, false),
            (wide, wide, false, false),
            (wide_outer, narrow_outer, false, true),
            (narrow_outer, wide_outer, true, false),
        ] {
            let pairs = || SchemaPairs::new();
            assert_eq!(schemas_add_fields(ctx, source, target, &mut pairs()), adds);
            assert_eq!(schemas_add_fields(ctx, target, source, &mut pairs()), drops);
        }
        Ok(())
    }
}
