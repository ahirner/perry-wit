use std::collections::{HashMap, HashSet};

use pliron::builtin::ops::FuncOp;
use pliron::context::{Context, Ptr};
use pliron::irbuild::{
    inserter::{Inserter, OpInsertionPoint},
    listener::DummyListener,
    rewriter::{IRRewriter, Rewriter},
};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::pass::{AnalysisManager, PassResult};
use pliron::result::Result;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use pliron::value::Value;

use crate::JsonSchema;
use crate::attrs::Absence;
use crate::calls::CallOp;
use crate::control::{BrOp, CondBrOp};
use crate::intrinsics::{Intrinsic, JsonStringifyOp, intrinsic_of};
use crate::optional::{UnwrapOp, absence, present_type};
use crate::types::RecordType;

/// MIR pass that recognizes `JSON.parse<T>(JSON.stringify(value))` and rewrites
/// provably compatible schemas into a schema-driven deep-copy and normalization helper.
pub(crate) fn normalize_json_copy(
    root: Ptr<Operation>,
    ctx: &mut Context,
    analyses: &mut AnalysisManager,
) -> Result<PassResult> {
    let facts = analyses.get_analysis::<crate::facts::ProgramFacts>(root, ctx)?;
    let mut rewrites = 0u64;
    let mut missed = 0u64;

    for func in crate::aggregates::functions(root, ctx) {
        for site in find_parse_stringify_sites(func, ctx) {
            let source_ty = site.source_value.get_type(ctx);
            if can_rewrite_parse_stringify(ctx, &facts, func, &site, source_ty, site.target_ty) {
                rewrite_site(ctx, &site);
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
    result.ir_changed = (rewrites != 0).into();
    Ok(result)
}

/// A `JSON.parse(JSON.stringify(source))` site. The rewrite copies the source at the consumer,
/// which moves the stringify, and any exception producing the text raises, to the consumer.
pub(crate) struct ParseStringifySite {
    pub(crate) stringify_op: Ptr<Operation>,
    pub(crate) unwrap_op: Option<Ptr<Operation>>,
    pub(crate) consumer_op: Ptr<Operation>,
    pub(crate) intermediate_parse_op: Option<Ptr<Operation>>,
    pub(crate) source_value: Value,
    target_ty: TypeHandle,
    /// Whether the stringify or its unwrap may throw.
    pub(crate) text_may_throw: bool,
}

impl ParseStringifySite {
    pub(crate) fn new(
        ctx: &Context,
        source_value: Value,
        (stringify_op, unwrap_op): (Ptr<Operation>, Option<Ptr<Operation>>),
        intermediate_parse_op: Option<Ptr<Operation>>,
        consumer_op: Ptr<Operation>,
    ) -> Self {
        let source_ty = source_value.get_type(ctx);
        // Stringification throws on a cycle; its result is undefined only for an absent value.
        let stringify_throws =
            crate::may_throw(ctx, stringify_op) && may_be_cyclic(ctx, source_ty, &mut Vec::new());
        let unwrap_throws = unwrap_op.is_some_and(|unwrap| crate::may_throw(ctx, unwrap))
            && (absence(ctx, source_ty).is_some()
                || source_ty.deref(ctx).is::<crate::types::JsonType>());
        Self {
            stringify_op,
            unwrap_op,
            consumer_op,
            intermediate_parse_op,
            source_value,
            target_ty: consumer_op.deref(ctx).get_result(0).get_type(ctx),
            text_may_throw: stringify_throws || unwrap_throws,
        }
    }
}

/// Whether a `ty` value may reach itself. `path` holds the enclosing types; a JSON value or a
/// type without a JSON schema may.
fn may_be_cyclic(ctx: &Context, ty: TypeHandle, path: &mut Vec<TypeHandle>) -> bool {
    let ty = present_type(ctx, ty);
    if path.contains(&ty) {
        return true;
    }
    if matches!(JsonSchema::of(ctx, ty), None | Some(JsonSchema::Value)) {
        return true;
    }
    path.push(ty);
    let cyclic = crate::types::type_children(&*ty.deref(ctx))
        .is_none_or(|mut children| children.any(|child| may_be_cyclic(ctx, child, path)));
    path.pop();
    cyclic
}

fn find_parse_stringify_sites(func: FuncOp, ctx: &Context) -> Vec<ParseStringifySite> {
    crate::aggregates::operations(ctx, &func)
        .into_iter()
        .filter_map(|op| parse_stringify_site(ctx, op))
        .collect()
}

/// The site `op` consumes, when it decodes `JSON.parse(JSON.stringify(value))`: a decoder of
/// text directly, or a value decoder of the parsed JSON.
fn parse_stringify_site(ctx: &Context, op: Ptr<Operation>) -> Option<ParseStringifySite> {
    if !is_unary(ctx, op) {
        return None;
    }
    let arg = op.deref(ctx).get_operand(0);
    let (intermediate_parse_op, text) = if is_value_decoder(ctx, op) {
        let parse_op = arg.defining_op()?;
        let parses = intrinsic_of(ctx, parse_op) == Some(Intrinsic::JsonParse);
        (parses && parse_op.deref(ctx).get_num_operands() == 1)
            .then(|| (Some(parse_op), parse_op.deref(ctx).get_operand(0)))?
    } else if is_text_decoder(ctx, op)
        || intrinsic_of(ctx, op) == Some(Intrinsic::JsonParse)
            // A parse a value decoder consumes is matched from that decoder instead.
            && !op.deref(ctx).get_result(0).uses(ctx).iter().any(|use_site| {
                let user = use_site.user_op();
                is_unary(ctx, user) && is_value_decoder(ctx, user)
            })
    {
        (None, arg)
    } else {
        return None;
    };
    let (stringify_op, unwrap_op, source_value) = match_unwrapped_stringify(ctx, text)?;
    Some(ParseStringifySite::new(
        ctx,
        source_value,
        (stringify_op, unwrap_op),
        intermediate_parse_op,
        op,
    ))
}

fn is_unary(ctx: &Context, op: Ptr<Operation>) -> bool {
    let operation = op.deref(ctx);
    operation.get_num_operands() == 1 && operation.get_num_results() == 1
}

fn is_text_decoder(ctx: &Context, op: Ptr<Operation>) -> bool {
    Operation::is_op::<crate::JsonDecodeOp>(op, ctx)
        && op
            .deref(ctx)
            .get_operand(0)
            .get_type(ctx)
            .deref(ctx)
            .is::<crate::StringType>()
}

/// Whether `op` decodes a parsed JSON value or performs numeric coercion.
fn is_value_decoder(ctx: &Context, op: Ptr<Operation>) -> bool {
    Operation::is_op::<crate::JsonDecodeOp>(op, ctx) && !is_text_decoder(ctx, op)
        || intrinsic_of(ctx, op) == Some(Intrinsic::JsonToNumber)
}

fn match_unwrapped_stringify(
    ctx: &Context,
    text_val: Value,
) -> Option<(Ptr<Operation>, Option<Ptr<Operation>>, Value)> {
    let def_op = text_val.defining_op()?;
    let (stringify_op, unwrap_op) =
        if Operation::is_op::<UnwrapOp>(def_op, ctx) && def_op.deref(ctx).get_num_operands() == 1 {
            (
                def_op.deref(ctx).get_operand(0).defining_op()?,
                Some(def_op),
            )
        } else {
            (def_op, None)
        };
    (Operation::is_op::<JsonStringifyOp>(stringify_op, ctx)
        && stringify_op.deref(ctx).get_num_operands() == 1)
        .then(|| {
            (
                stringify_op,
                unwrap_op,
                stringify_op.deref(ctx).get_operand(0),
            )
        })
}

fn can_rewrite_parse_stringify(
    ctx: &Context,
    facts: &crate::facts::ProgramFacts,
    func: FuncOp,
    site: &ParseStringifySite,
    source_ty: TypeHandle,
    target_ty: TypeHandle,
) -> bool {
    if [
        Some(site.stringify_op),
        site.unwrap_op,
        site.intermediate_parse_op,
    ]
    .into_iter()
    .flatten()
    .any(|op| op.deref(ctx).get_result(0).uses(ctx).len() != 1)
    {
        return false;
    }

    let Some(plan) = JsonCopyPlan::get(ctx, source_ty, target_ty) else {
        return false;
    };
    let consumer = site.consumer_op.deref(ctx).get_result(0);
    let stringify = value_reaches_stringify(ctx, consumer);
    if stringify && plan.drops_fields
        || (stringify || plan.adds_fields)
            && facts.may_have_json_residue(ctx, &func, site.source_value)
    {
        return false;
    }
    !facts.may_prevent_json_copy_between(ctx, &func, site)
}

/// Whether a `ty` value may hold a record. `seen` holds the types already visited, so a
/// recursive type ends its cycle: revisiting adds no new answer to an `any`.
fn type_may_contain_record(ctx: &Context, ty: TypeHandle, seen: &mut HashSet<TypeHandle>) -> bool {
    if !seen.insert(ty) {
        return false;
    }
    let ty = present_type(ctx, ty);
    let Some(schema) = JsonSchema::of(ctx, ty) else {
        return false;
    };
    match schema {
        JsonSchema::Value => true,
        JsonSchema::Record(rec) if !rec.deref(ctx).is_tuple() => true,
        _ => crate::types::type_children(&*ty.deref(ctx)).is_none_or(|mut children| {
            children.any(|child| type_may_contain_record(ctx, child, seen))
        }),
    }
}

fn value_reaches_stringify(ctx: &Context, root: Value) -> bool {
    let may_contain_record = |ty| type_may_contain_record(ctx, ty, &mut HashSet::new());
    if !may_contain_record(root.get_type(ctx)) {
        return false;
    }
    let mut worklist = vec![root];
    let mut visited = HashSet::new();
    while let Some(val) = worklist.pop() {
        if !visited.insert(val) || !may_contain_record(val.get_type(ctx)) {
            continue;
        }
        for use_site in val.uses(ctx) {
            let user = use_site.user_op();
            if Operation::get_op::<JsonStringifyOp>(user, ctx).is_some()
                || Operation::get_op::<CallOp>(user, ctx).is_some()
            {
                return true;
            }
            let op_ref = user.deref(ctx);
            if matches!(
                intrinsic_of(ctx, user),
                Some(Intrinsic::JsonSpread | Intrinsic::JsonOrderCopy)
            ) && op_ref.get_num_operands() >= 2
                && val == op_ref.get_operand(1)
            {
                worklist.push(op_ref.get_operand(0));
            }
            if Operation::get_op::<BrOp>(user, ctx).is_some()
                || Operation::get_op::<CondBrOp>(user, ctx).is_some()
            {
                for dest in op_ref.successors() {
                    worklist.extend(dest.deref(ctx).arguments());
                }
            }
            worklist.extend(op_ref.results());
        }
    }
    false
}

type TypePair = (TypeHandle, TypeHandle);

pub enum CopySlot {
    Payload(TypePair),
    Field(TypePair, TypedHandle<RecordType>, usize),
}

impl CopySlot {
    pub fn types(&self) -> TypePair {
        match *self {
            Self::Payload(pair) | Self::Field(pair, ..) => pair,
        }
    }
}

#[derive(Default)]
pub struct JsonCopyPlan {
    slots: HashMap<TypePair, Vec<Option<CopySlot>>>,
    adds_fields: bool,
    drops_fields: bool,
}

impl JsonCopyPlan {
    /// Cache successful plans. An opaque record can acquire its definition later.
    pub fn get(ctx: &Context, source: TypeHandle, target: TypeHandle) -> Option<&Self> {
        let key = (source, target);
        let cell = &crate::CachedFact::intern(ctx, key).get(ctx).value;
        if let Some(plan) = cell.get() {
            return Some(plan);
        }
        let mut plan = Self::default();
        plan.visit(ctx, key)?;
        Some(cell.get_or_init(|| plan))
    }

    pub fn slots(&self, types: TypePair) -> Option<&[Option<CopySlot>]> {
        self.slots.get(&types).map(Vec::as_slice)
    }

    fn visit(&mut self, ctx: &Context, pair @ (source, target): TypePair) -> Option<()> {
        if self.slots.contains_key(&pair) {
            return Some(());
        }
        self.slots.insert(pair, Vec::new());
        let strip_undefined = |mut ty| {
            while let Some((inner, Absence::Undefined)) = crate::optional_parts(ctx, ty) {
                ty = inner;
            }
            ty
        };
        let (source, target) = (strip_undefined(source), strip_undefined(target));
        let source_absence = absence(ctx, source);
        let target_absence = absence(ctx, target);
        if source_absence == Some(Absence::Both)
            || target_absence == Some(Absence::Both)
            || source_absence == Some(Absence::Null) && target_absence != source_absence
        {
            return None;
        }
        let (source, target) = (present_type(ctx, source), present_type(ctx, target));
        let schemas = (JsonSchema::of(ctx, source), JsonSchema::of(ctx, target));
        let mut slots = Vec::new();
        match schemas {
            (Some(JsonSchema::Record(from)), Some(JsonSchema::Record(to))) => {
                let (src, dst) = (from.deref(ctx), to.deref(ctx));
                let (source_fields, target_fields) = (src.fields().ok()?, dst.fields().ok()?);
                if src.kind() != dst.kind()
                    || src.is_tuple() && source_fields.len() != target_fields.len()
                {
                    return None;
                }
                for (i, (name, ty)) in target_fields.iter().enumerate() {
                    let field = if src.kind() == crate::RecordKind::Tuple {
                        source_fields.get(i).map(|(_, ty)| (i, *ty))
                    } else {
                        src.field(name)
                    };
                    if let Some((index, source_ty)) = field {
                        slots.push(Some(CopySlot::Field((source_ty, *ty), from, index)));
                    } else {
                        if absence(ctx, *ty) != Some(Absence::Undefined) {
                            return None;
                        }
                        self.adds_fields = true;
                        self.visit(ctx, (*ty, *ty))?;
                        slots.push(None);
                    }
                }
                if !src.is_tuple() {
                    for (name, ty) in source_fields {
                        if dst.field(name).is_none() {
                            self.drops_fields = true;
                            self.visit(ctx, (*ty, *ty))?;
                        }
                    }
                }
            }
            (Some(JsonSchema::Array(s)), Some(JsonSchema::Array(t))) => {
                slots.push(Some(CopySlot::Payload((s, t))))
            }
            (
                Some(JsonSchema::Dictionary {
                    entry: s, value, ..
                }),
                Some(JsonSchema::Dictionary { entry: t, .. }),
            ) if !crate::admits_undefined(ctx, value) => {
                slots.push(Some(CopySlot::Payload((s.into(), t.into()))))
            }
            (Some(JsonSchema::Variant(s)), Some(JsonSchema::Variant(t))) if s == t => {
                let variant = s.deref(ctx);
                let mut shapes = HashSet::new();
                for (_, payload) in variant.cases() {
                    let child = if let Some(ty) = *payload {
                        if !shapes.insert(PayloadShape::of(ctx, ty))
                            || present_type(ctx, ty).deref(ctx).is::<crate::NumericType>()
                        {
                            return None;
                        }
                        Some(CopySlot::Payload((ty, ty)))
                    } else {
                        None
                    };
                    slots.push(child);
                }
            }
            _ if scalar_copy(ctx, source, target, schemas) => slots.push(None),
            _ => return None,
        }
        for slot in slots.iter().flatten() {
            self.visit(ctx, slot.types())?;
        }
        self.slots.insert(pair, slots);
        Some(())
    }
}

fn scalar_copy(
    ctx: &Context,
    source: TypeHandle,
    target: TypeHandle,
    schemas: (Option<JsonSchema>, Option<JsonSchema>),
) -> bool {
    let numeric = source.deref(ctx).is::<crate::NumericType>()
        || matches!(
            schemas.0,
            Some(
                JsonSchema::Number
                    | JsonSchema::Integer {
                        width: 8 | 16 | 32 | 64,
                        ..
                    }
            )
        );
    numeric
        && (target.deref(ctx).is::<crate::NumericType>()
            || matches!(schemas.1, Some(JsonSchema::Number)))
        || source == target
            && (numeric || matches!(schemas.0, Some(JsonSchema::Boolean | JsonSchema::String)))
}

#[derive(Eq, PartialEq, Hash)]
enum PayloadShape {
    Record,
    Sequence,
    Value(TypeHandle),
}

impl PayloadShape {
    fn of(ctx: &Context, ty: TypeHandle) -> Self {
        match JsonSchema::of(ctx, ty) {
            Some(JsonSchema::Record(_)) => Self::Record,
            Some(JsonSchema::Array(_) | JsonSchema::Dictionary { .. }) => Self::Sequence,
            _ => Self::Value(ty),
        }
    }
}

fn rewrite_site(ctx: &mut Context, site: &ParseStringifySite) {
    let mut rewriter = IRRewriter::<DummyListener>::default();
    rewriter.set_insertion_point(OpInsertionPoint::BeforeOperation(site.consumer_op));
    let call = crate::JsonCopyOp::new(ctx, site.source_value, site.target_ty);
    rewriter.insert_operation(ctx, call.get_operation());
    // Keep any CaughtOp immediately following `site.consumer_op` intact so it now
    // checks whether normalization raised an exception inside a try block.
    rewriter.replace_operation(ctx, site.consumer_op, call.get_operation());
    for op in [
        site.intermediate_parse_op,
        site.unwrap_op,
        Some(site.stringify_op),
    ]
    .into_iter()
    .flatten()
    {
        crate::exceptions::clear_caught(ctx, op);
        rewriter.erase_operation(ctx, op);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ArrayType, OptionalType};
    use pliron::builtin::types::FP64Type;

    #[test]
    fn schema_field_changes_are_directional() -> Result<()> {
        let ctx = &mut Context::new();
        let number: TypeHandle = FP64Type::get(ctx).into();
        let wide: TypeHandle = RecordType::get(
            ctx,
            pliron::ident!("Wide"),
            Some(vec![
                ("x".into(), number),
                ("y".into(), OptionalType::get(ctx, number).into()),
            ]),
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
            let plan = JsonCopyPlan::get(ctx, source, target).unwrap();
            assert_eq!((plan.adds_fields, plan.drops_fields), (adds, drops));
        }
        Ok(())
    }

    /// Only a recursive type lets the stringify throw on a cycle.
    #[test]
    fn recursive_sources_may_be_cyclic() -> Result<()> {
        let ctx = &mut Context::new();
        let number: TypeHandle = FP64Type::get(ctx).into();
        let leaf: TypeHandle = RecordType::get(
            ctx,
            pliron::ident!("Leaf"),
            Some(vec![("x".into(), number)]),
        )?
        .into();
        let node: TypeHandle = RecordType::get(ctx, pliron::ident!("Node"), None)?.into();
        assert!(JsonCopyPlan::get(ctx, node, node).is_none());
        let next = OptionalType::get(ctx, ArrayType::get(ctx, node).into()).into();
        RecordType::get(
            ctx,
            pliron::ident!("Node"),
            Some(vec![("next".into(), next)]),
        )?;
        assert!(JsonCopyPlan::get(ctx, node, node).is_some());
        let leaves = ArrayType::get(ctx, leaf).into();
        for (ty, cyclic) in [(leaf, false), (leaves, false), (node, true), (next, true)] {
            assert_eq!(may_be_cyclic(ctx, ty, &mut Vec::new()), cyclic);
        }
        Ok(())
    }
}
