//! Built-in expression, method, property, constructor, and callback typing over dense `Ty`.
//!
//! Operates exclusively on `Ty` indices without cloning `HirType` trees or delegating composite
//! expressions to `perry_hir::infer_expr_type`.
use super::types::{NameId, ObjectProp, Ty, TyKind, TypeTable};
use crate::{
    Absence,
    collections::IteratorMethod,
    methods::{Kind, Method, MethodResult, Receiver},
    shapes::{self, Declarations},
};
use perry_hir::{
    analysis::{HirTypeFacts, infer_expr_type},
    ir::{BinaryOp, Expr, LogicalOp, UnaryOp},
    types::{FuncId, GlobalId, LocalId, Type as HirType},
};
use pliron::context::Context;

struct NoFacts;
impl HirTypeFacts for NoFacts {
    fn local_type(&self, _: LocalId) -> Option<&HirType> {
        None
    }
    fn global_type(&self, _: GlobalId) -> Option<&HirType> {
        None
    }
    fn function_return_type(&self, _: FuncId) -> Option<&HirType> {
        None
    }
}

pub(crate) const SETTLED: &str = "PromiseSettledResult";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PredicateResult {
    Filter,
    Bool,
    Index,
    Find,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CallbackContract {
    Void,
    Mapped { keep_holes: bool },
    FlatMapped,
    Predicate(PredicateResult),
    Comparator,
    Fold,
    Replacement,
    GroupKey,
}

/// Normalize a leaf `HirType::Named` integer carrier (64-bit -> BigInt, <=32-bit -> Number).
pub(crate) fn normalize_named_integer(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &TypeTable,
    ty: Ty,
) -> Ty {
    if let TyKind::Leaf(id) = types.kind(ty)
        && let HirType::Named(name) = id.hir(ctx)
        && let Some(carrier) = decls.integer_carrier(name)
    {
        return match carrier {
            HirType::BigInt => Ty::BIGINT,
            _ => Ty::NUMBER,
        };
    }
    ty
}

/// Map a `Ty` to a built-in [`Receiver`] family for method/property lookup.
pub(crate) fn receiver_of(ctx: &Context, types: &TypeTable, ty: Ty) -> Option<Receiver> {
    match types.kind(ty) {
        TyKind::Array(_) | TyKind::Tuple(_) => Some(Receiver::Value(Kind::Array)),
        TyKind::Cell { kind, .. } if kind.is_array() => Some(Receiver::Value(Kind::Array)),
        TyKind::String => Some(Receiver::Value(Kind::String)),
        TyKind::Number | TyKind::Int32 => Some(Receiver::Value(Kind::Number)),
        TyKind::Boolean => Some(Receiver::Value(Kind::Boolean)),
        TyKind::BigInt => Some(Receiver::Value(Kind::BigInt)),
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::StringLiteral(_) => Some(Receiver::Value(Kind::String)),
            HirType::Named(name) => receiver_for_name(name),
            _ => None,
        },
        TyKind::Generic { base, .. } => match base.hir(ctx) {
            HirType::Named(name) => receiver_for_name(name),
            _ => None,
        },
        _ => None,
    }
}

fn receiver_for_name(name: &str) -> Option<Receiver> {
    match name {
        "Map" | "ReadonlyMap" => Some(Receiver::Map),
        "Set" | "ReadonlySet" => Some(Receiver::Set),
        "Stats" | "fs.Stats" => Some(Receiver::Value(Kind::Stats)),
        other => Some(Receiver::Value(Kind::builtin(other)?)),
    }
}

/// Look up a built-in method/property result rule for a `Ty` receiver.
pub(crate) fn lookup_method_result(
    ctx: &Context,
    types: &TypeTable,
    receiver: Ty,
    name: &str,
    property: bool,
) -> Option<MethodResult> {
    if let Some(recv) = receiver_of(ctx, types, receiver) {
        return Method::lookup(recv, name)
            .filter(|m| m.is_property() == property)
            .map(Method::result_rule);
    }
    if property {
        return None;
    }
    let hir = types.materialize_hir(ctx, receiver, &|_| None);
    Method::inferred(&hir, name, false)
}

/// Extract the iterated element type of an array, string, tuple, Set, Map, or typed array.
pub(crate) fn iterated_element(ctx: &Context, types: &mut TypeTable, ty: Ty) -> Option<Ty> {
    let (ty, _) = types.strip_nullish(ty);
    match types.kind(ty) {
        TyKind::String => Some(Ty::STRING),
        TyKind::Array(elem) => Some(elem),
        TyKind::Tuple(span) => {
            let items: Vec<Ty> = types.tuple_slice(span).to_vec();
            Some(types.union(&items))
        }
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::StringLiteral(_) => Some(Ty::STRING),
            HirType::Named(name) if name == "Uint8Array" => Some(Ty::NUMBER),
            _ => None,
        },
        TyKind::Generic { base, args } => {
            let HirType::Named(base_name) = base.hir(ctx) else {
                return None;
            };
            let slice: Vec<Ty> = types.generic_slice(args).to_vec();
            match (base_name.as_str(), slice.as_slice()) {
                ("Set" | "ReadonlySet" | "SetValues" | "Array" | "ReadonlyArray", [elem]) => {
                    Some(*elem)
                }
                ("MapKeys", [key, _]) => Some(*key),
                ("MapValues", [_, val]) => Some(*val),
                ("Map" | "ReadonlyMap" | "MapEntries", [key, val]) => {
                    Some(types.tuple(&[*key, *val]))
                }
                ("SetEntries", [elem]) => Some(types.tuple(&[*elem, *elem])),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Result type of `.keys()`, `.values()`, `.entries()` on Map, Set, or Array.
pub(crate) fn iterator_view(
    ctx: &Context,
    types: &mut TypeTable,
    source: Ty,
    method: IteratorMethod,
) -> Option<Ty> {
    let (source, _) = types.strip_nullish(source);
    match types.kind(source) {
        TyKind::Array(elem) => {
            let out_elem = match method {
                IteratorMethod::Keys => Ty::NUMBER,
                IteratorMethod::Values => elem,
                IteratorMethod::Entries => types.tuple(&[Ty::NUMBER, elem]),
            };
            Some(types.array(out_elem))
        }
        TyKind::Generic { base, args } => {
            let HirType::Named(base_name) = base.hir(ctx) else {
                return None;
            };
            let slice: Vec<Ty> = types.generic_slice(args).to_vec();
            match base_name.as_str() {
                "Map" | "ReadonlyMap" if slice.len() == 2 => {
                    let view_base = shapes::TypeId::from_owned(
                        ctx,
                        HirType::Named(method.map_view().type_name().to_owned()),
                    );
                    Some(types.generic(view_base, &slice))
                }
                "Set" | "ReadonlySet" if !slice.is_empty() => {
                    let view_base = shapes::TypeId::from_owned(
                        ctx,
                        HirType::Named(method.set_view().type_name().to_owned()),
                    );
                    Some(types.generic(view_base, &slice[..1]))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Arguments to [`apply_method_result`].
#[derive(Clone, Copy)]
pub(crate) struct MethodResultArgs<'a> {
    pub(crate) receiver: Ty,
    pub(crate) arg0: Option<Ty>,
    pub(crate) arg0_expr: Option<&'a Expr>,
    pub(crate) callback_result: Option<Ty>,
    pub(crate) sparse_arrays: bool,
}

impl MethodResultArgs<'_> {
    pub(crate) fn receiver_only(receiver: Ty) -> Self {
        Self {
            receiver,
            arg0: None,
            arg0_expr: None,
            callback_result: None,
            sparse_arrays: false,
        }
    }
}

/// Evaluate the result type of a [`MethodResult`] rule given already-typed receiver and callback/arg types.
pub(crate) fn apply_method_result(
    ctx: &Context,
    types: &mut TypeTable,
    rule: MethodResult,
    call: MethodResultArgs<'_>,
) -> Ty {
    let MethodResultArgs {
        receiver,
        arg0,
        arg0_expr,
        callback_result,
        sparse_arrays,
    } = call;
    match rule {
        MethodResult::Mapped => match callback_result {
            Some(ret) if !types.is_open(ret) => {
                let elem = if sparse_arrays {
                    types.with_absences(ret, Absence::Undefined)
                } else {
                    ret
                };
                types.array(elem)
            }
            _ => Ty::OPEN,
        },
        MethodResult::FlatMapped => match callback_result {
            Some(ret) if !types.is_open(ret) => match types.kind(ret) {
                TyKind::Array(_) => ret,
                _ => types.array(ret),
            },
            _ => Ty::OPEN,
        },
        MethodResult::Receiver => receiver,
        MethodResult::Flat => {
            let zero_depth = matches!(arg0_expr, Some(Expr::Number(n)) if *n <= 0.0)
                || matches!(arg0_expr, Some(Expr::Integer(n)) if *n <= 0);
            let (stripped, _) = types.strip_nullish(receiver);
            match types.kind(stripped) {
                TyKind::Array(inner) if !zero_depth => {
                    let (inner_def, states) = types.strip_nullish(inner);
                    let elem = match types.kind(inner_def) {
                        TyKind::Array(e) => e,
                        _ => inner_def,
                    };
                    let with_states = types.with_absences(elem, states);
                    types.array(with_states)
                }
                _ => stripped,
            }
        }
        MethodResult::Element | MethodResult::At => {
            let (stripped, _) = types.strip_nullish(receiver);
            match types.kind(stripped) {
                TyKind::Array(elem) => types.with_absences(elem, Absence::Undefined),
                TyKind::Tuple(span) => {
                    let items: Vec<Ty> = types.tuple_slice(span).to_vec();
                    let u = types.union(&items);
                    types.with_absences(u, Absence::Undefined)
                }
                TyKind::String if rule == MethodResult::At => {
                    types.with_absences(Ty::STRING, Absence::Undefined)
                }
                _ => Ty::OPEN,
            }
        }
        MethodResult::Reduced => {
            if let Some(ret) = callback_result
                && !types.is_open(ret)
                && ret != Ty::VOID
            {
                return ret;
            }
            if let Some(init_ty) = arg0
                && !types.is_open(init_ty)
            {
                return init_ty;
            }
            let (stripped, _) = types.strip_nullish(receiver);
            match types.kind(stripped) {
                TyKind::Array(elem) => elem,
                _ => Ty::OPEN,
            }
        }
        MethodResult::Void => Ty::VOID,
        MethodResult::String => Ty::STRING,
        MethodResult::Strings => types.array(Ty::STRING),
        MethodResult::NullableStrings => {
            let opt_str = types.with_absences(Ty::STRING, Absence::Undefined);
            let arr = types.array(opt_str);
            types.with_absences(arr, Absence::Null)
        }
        MethodResult::Number => Ty::NUMBER,
        MethodResult::Boolean => Ty::BOOLEAN,
        MethodResult::OptionalNumber => types.with_absences(Ty::NUMBER, Absence::Undefined),
        MethodResult::NullableString => types.with_absences(Ty::STRING, Absence::Null),
        MethodResult::Native(kind) => native_ty(ctx, types, kind),
        MethodResult::Nullable(kind) => {
            let inner = native_ty(ctx, types, kind);
            types.with_absences(inner, Absence::Null)
        }
        MethodResult::Optional(kind) => {
            let inner = native_ty(ctx, types, kind);
            types.with_absences(inner, Absence::Undefined)
        }
        MethodResult::Promise(kind) => {
            let inner = native_ty(ctx, types, kind);
            types.promise(inner)
        }
        MethodResult::PromiseVoid => types.promise(Ty::VOID),
        MethodResult::PromiseJson => types.promise(Ty::DYNAMIC),
        MethodResult::StreamReader => {
            let is_byob = arg0_expr.is_some_and(|arg| match arg {
                Expr::Object(props) => props
                    .iter()
                    .any(|(k, v)| k == "mode" && matches!(v, Expr::String(s) if s == "byob")),
                Expr::New { args, .. } => args
                    .iter()
                    .any(|v| matches!(v, Expr::String(s) if s == "byob")),
                _ => false,
            });
            native_ty(
                ctx,
                types,
                if is_byob {
                    Kind::ByobReader
                } else {
                    Kind::Reader
                },
            )
        }
        MethodResult::IteratorKeys => {
            iterator_view(ctx, types, receiver, IteratorMethod::Keys).unwrap_or(Ty::OPEN)
        }
        MethodResult::IteratorValues => {
            iterator_view(ctx, types, receiver, IteratorMethod::Values).unwrap_or(Ty::OPEN)
        }
        MethodResult::IteratorEntries => {
            iterator_view(ctx, types, receiver, IteratorMethod::Entries).unwrap_or(Ty::OPEN)
        }
        MethodResult::Lookup => {
            let (stripped, _) = types.strip_nullish(receiver);
            if let TyKind::Generic { base, args } = types.kind(stripped)
                && let HirType::Named(base_name) = base.hir(ctx)
                && matches!(base_name.as_str(), "Map" | "ReadonlyMap")
                && let [_, val] = types.generic_slice(args)
            {
                let val = *val;
                types.with_absences(val, Absence::Undefined)
            } else {
                Ty::OPEN
            }
        }
        MethodResult::Duration => duration_ty(types),
    }
}

pub(crate) fn native_ty(ctx: &Context, types: &mut TypeTable, kind: Kind) -> Ty {
    match kind {
        Kind::String => Ty::STRING,
        Kind::Number => Ty::NUMBER,
        Kind::Boolean => Ty::BOOLEAN,
        Kind::BigInt => Ty::BIGINT,
        other => types.named(ctx, other.label()),
    }
}

fn duration_ty(types: &mut TypeTable) -> Ty {
    let props = [
        ("blank", Ty::BOOLEAN),
        ("days", Ty::NUMBER),
        ("months", Ty::NUMBER),
        ("sign", Ty::NUMBER),
        ("weeks", Ty::NUMBER),
        ("years", Ty::NUMBER),
    ]
    .map(|(name, ty)| ObjectProp {
        name: types.intern_name(name),
        ty,
        optional: false,
        readonly: false,
    });
    types.object(&props, None)
}

/// Infer the result type of a binary operator from its already-typed operands.
pub(crate) fn binary_result(
    ctx: &Context,
    types: &mut TypeTable,
    op: BinaryOp,
    left: Ty,
    right: Ty,
) -> Ty {
    match op {
        BinaryOp::Add => {
            if is_string_ty(ctx, types, left) || is_string_ty(ctx, types, right) {
                return Ty::STRING;
            }
            if left == Ty::BOTTOM || right == Ty::BOTTOM {
                return Ty::BOTTOM;
            }
            if left == Ty::BIGINT && right == Ty::BIGINT {
                return Ty::BIGINT;
            }
            if is_mixed_string_union(ctx, types, left) || is_mixed_string_union(ctx, types, right) {
                return types.union(&[Ty::NUMBER, Ty::STRING]);
            }
            Ty::NUMBER
        }
        BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div
        | BinaryOp::Mod
        | BinaryOp::Pow
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr
        | BinaryOp::UShr => {
            if left == Ty::BIGINT && right == Ty::BIGINT {
                return Ty::BIGINT;
            }
            Ty::NUMBER
        }
    }
}

pub(crate) fn unary_result(op: UnaryOp, operand: Ty) -> Ty {
    match op {
        UnaryOp::Not => Ty::BOOLEAN,
        UnaryOp::Neg | UnaryOp::Pos | UnaryOp::BitNot => {
            if operand == Ty::BIGINT {
                Ty::BIGINT
            } else {
                Ty::NUMBER
            }
        }
    }
}

fn is_string_ty(ctx: &Context, types: &TypeTable, ty: Ty) -> bool {
    match types.kind(ty) {
        TyKind::String => true,
        TyKind::Leaf(id) => matches!(id.hir(ctx), HirType::StringLiteral(_)),
        _ => false,
    }
}

fn is_numeric_atom(ty: Ty) -> bool {
    matches!(
        ty,
        Ty::NUMBER | Ty::INT32 | Ty::BOOLEAN | Ty::NULL | Ty::VOID
    )
}

fn is_mixed_string_union(ctx: &Context, types: &TypeTable, ty: Ty) -> bool {
    let TyKind::Union(span) = types.kind(ty) else {
        return false;
    };
    types.union_slice(span).iter().any(|&m| {
        !is_numeric_atom(m)
            && (is_string_ty(ctx, types, m)
                || !matches!(types.kind(m), TyKind::Bottom | TyKind::Open))
    })
}

/// Classify a leaf/intrinsic HIR expression into [`Ty`].
///
/// Fast-paths common literals and Perry-WIT nullish/JSON refinements; all remaining leaf
/// intrinsics reuse `perry_hir::analysis::infer_expr_type` with empty facts.
pub(crate) fn leaf_expr_type(ctx: &Context, types: &mut TypeTable, expr: &Expr) -> Ty {
    match expr {
        Expr::Undefined | Expr::Void(_) => Ty::VOID,
        Expr::Null => Ty::NULL,
        Expr::Bool(_) | Expr::Compare { .. } | Expr::InstanceOf { .. } | Expr::In { .. } => {
            Ty::BOOLEAN
        }
        Expr::Integer(_) | Expr::Number(_) | Expr::Update { .. } => Ty::NUMBER,
        Expr::BigInt(_) => Ty::BIGINT,
        Expr::String(_) | Expr::WtfString(_) | Expr::TypeOf(_) => Ty::STRING,
        // Perry-WIT nullish and JSON refinements over `perry-hir`.
        Expr::Uint8ArrayGet { .. } | Expr::BufferIndexGet { .. } => {
            types.with_absences(Ty::NUMBER, Absence::Undefined)
        }
        Expr::StringAt { .. } => types.with_absences(Ty::STRING, Absence::Undefined),
        Expr::DateToJSON(_) => types.with_absences(Ty::STRING, Absence::Null),
        Expr::JsonParse(_) | Expr::JsonParseWithReviver(..) | Expr::JsonParseReviver { .. } => {
            Ty::DYNAMIC
        }
        Expr::JsonParseTyped { ty, .. } => {
            let parsed = types.intern_hir(ctx, ty, true);
            if parsed == Ty::OPEN {
                Ty::DYNAMIC
            } else {
                parsed
            }
        }
        _ => {
            let hir = infer_expr_type(expr, &NoFacts);
            if matches!(
                hir,
                HirType::Any | HirType::Unknown | HirType::Object(_) | HirType::Generic { .. }
            ) {
                Ty::OPEN
            } else {
                let ty = types.intern_hir(ctx, &hir, false);
                if types.is_open(ty) { Ty::OPEN } else { ty }
            }
        }
    }
}

fn dict_value_ty(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &mut TypeTable,
    hir: &HirType,
) -> Option<Ty> {
    let (_, val_hir) = decls.dict_entry_types(ctx, hir)?;
    let val_ty = types.intern_hir(ctx, &val_hir, true);
    Some(types.with_absences(val_ty, Absence::Undefined))
}

/// Property lookup on built-in, structural object, or declared nominal types.
pub(crate) fn property_type(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &mut TypeTable,
    source: Ty,
    property: &str,
) -> Option<Ty> {
    let (stripped, _) = types.strip_nullish(source);
    match types.kind(stripped) {
        TyKind::Dynamic => Some(Ty::DYNAMIC),
        TyKind::String if property == "length" => Some(Ty::NUMBER),
        TyKind::Array(_) | TyKind::Tuple(_) => match property {
            "length" | "index" => Some(Ty::NUMBER),
            "input" => Some(Ty::STRING),
            "groups" => {
                let opt_str = types.with_absences(Ty::STRING, Absence::Undefined);
                let rec = types.object(&[], Some(opt_str));
                Some(types.with_absences(rec, Absence::Undefined))
            }
            _ => None,
        },
        TyKind::Cell { kind, .. } if kind.is_array() => match property {
            "length" | "index" => Some(Ty::NUMBER),
            _ => None,
        },
        TyKind::Object { props, index } => {
            let target_name = types.intern_name(property);
            let found = types
                .object_slice(props)
                .iter()
                .find(|p| p.name == target_name)
                .copied();
            if let Some(prop) = found {
                return Some(if prop.optional {
                    types.with_absences(prop.ty, Absence::Undefined)
                } else {
                    prop.ty
                });
            }
            index.map(|idx| types.with_absences(idx, Absence::Undefined))
        }
        TyKind::Generic { base, args } => {
            let HirType::Named(base_name) = base.hir(ctx) else {
                return None;
            };
            if matches!(
                base_name.as_str(),
                "Map" | "ReadonlyMap" | "Set" | "ReadonlySet"
            ) && property == "size"
            {
                return Some(Ty::NUMBER);
            }
            if base_name == SETTLED && !decls.is_user_type(base_name) {
                let slice: Vec<Ty> = types.generic_slice(args).to_vec();
                return match (property, slice.as_slice()) {
                    ("status", _) => Some(Ty::STRING),
                    ("value", [val]) => Some(types.with_absences(*val, Absence::Undefined)),
                    ("reason", _) => {
                        let err = types.named(ctx, "Error");
                        Some(types.with_absences(err, Absence::Undefined))
                    }
                    _ => None,
                };
            }
            let arg_hirs: Vec<HirType> = types
                .generic_slice(args)
                .iter()
                .map(|&t| types.materialize_hir(ctx, t, &|_| None))
                .collect();
            if let Some(prop_hir) = decls.property_type(ctx, base_name, &arg_hirs, property) {
                let ty = types.intern_hir(ctx, &prop_hir, true);
                return Some(normalize_named_integer(ctx, decls, types, ty));
            }
            let generic_hir = HirType::Generic {
                base: base_name.clone(),
                type_args: arg_hirs,
            };
            if let Some(val_ty) = dict_value_ty(ctx, decls, types, &generic_hir) {
                return Some(val_ty);
            }
            lookup_method_result(ctx, types, stripped, property, true).map(|rule| {
                apply_method_result(ctx, types, rule, MethodResultArgs::receiver_only(stripped))
            })
        }
        TyKind::Leaf(id) => {
            let HirType::Named(base_name) = id.hir(ctx) else {
                return (matches!(id.hir(ctx), HirType::StringLiteral(_)) && property == "length")
                    .then_some(Ty::NUMBER);
            };
            if Kind::builtin(base_name) == Some(Kind::Exception) {
                match property {
                    "message" | "name" => return Some(Ty::STRING),
                    "cause" => return Some(Ty::DYNAMIC),
                    _ => {}
                }
            }
            match (base_name.as_str(), property) {
                ("Temporal.PlainDate", "year" | "month" | "day" | "dayOfWeek")
                | (
                    "Temporal.PlainDateTime",
                    "year" | "month" | "day" | "dayOfWeek" | "hour" | "minute" | "second"
                    | "millisecond" | "microsecond" | "nanosecond",
                )
                | ("Temporal.Instant", "epochMilliseconds") => return Some(Ty::NUMBER),
                ("Temporal.PlainDate" | "Temporal.PlainDateTime", "calendarId") => {
                    return Some(Ty::STRING);
                }
                _ => {}
            }
            if let Some(prop_hir) = decls.property_type(ctx, base_name, &[], property) {
                let ty = types.intern_hir(ctx, &prop_hir, true);
                return Some(normalize_named_integer(ctx, decls, types, ty));
            }
            if let Some(resolved) = decls.resolve_foreign_type(base_name) {
                let resolved_ty = types.intern_hir(ctx, resolved, true);
                if resolved_ty != stripped
                    && let Some(found) = property_type(ctx, decls, types, resolved_ty, property)
                {
                    return Some(found);
                }
            }
            if let Some(val_ty) =
                dict_value_ty(ctx, decls, types, &HirType::Named(base_name.clone()))
            {
                return Some(val_ty);
            }
            Method::named_property(base_name, property)
                .or_else(|| lookup_method_result(ctx, types, stripped, property, true))
                .map(|rule| {
                    apply_method_result(ctx, types, rule, MethodResultArgs::receiver_only(stripped))
                })
        }
        TyKind::Union(span) => {
            // Property read on a finite value union or discriminated tagged union:
            // collect property types from every member that defines the property
            // (after a tag check like `if (r.ok) r.value`, only the matching variant has it).
            let members: Vec<Ty> = types.union_slice(span).to_vec();
            let mut results = Vec::with_capacity(members.len());
            for member in members {
                if let Some(ty) = property_type(ctx, decls, types, member, property) {
                    results.push(ty);
                }
            }
            (!results.is_empty()).then(|| types.union(&results))
        }
        _ => None,
    }
}

/// Normalize a property name lookup inside `Object` shapes.
pub(crate) fn object_property(types: &TypeTable, ty: Ty, name: NameId) -> Option<(Ty, bool)> {
    let TyKind::Object { props, .. } = types.kind(ty) else {
        return None;
    };
    types
        .object_slice(props)
        .iter()
        .find(|p| p.name == name)
        .map(|p| (p.ty, p.optional))
}

/// Evaluate `object[index]` on a `Ty`.
pub(crate) fn index_type(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &mut TypeTable,
    object: Ty,
    index: &Expr,
) -> Option<Ty> {
    let (stripped, _) = types.strip_nullish(object);
    match types.kind(stripped) {
        TyKind::Dynamic => Some(Ty::DYNAMIC),
        TyKind::Tuple(span) => {
            let pos = match index {
                Expr::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
                Expr::Integer(n) => usize::try_from(*n).ok(),
                _ => None,
            };
            let slice = types.tuple_slice(span);
            match pos {
                Some(i) => Some(slice.get(i).copied().unwrap_or(Ty::VOID)),
                None => {
                    let items: Vec<Ty> = slice.to_vec();
                    Some(types.union(&items))
                }
            }
        }
        TyKind::Array(elem) => Some(elem),
        TyKind::String => Some(Ty::STRING),
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::Named(name) if name == "Uint8Array" => {
                Some(types.with_absences(Ty::NUMBER, Absence::Undefined))
            }
            HirType::Named(base) => dict_value_ty(ctx, decls, types, &HirType::Named(base.clone()))
                .or_else(|| match decls.resolve_foreign_type(base) {
                    Some(HirType::Array(elem)) => Some(types.intern_hir(ctx, elem, true)),
                    _ => None,
                }),
            _ => None,
        },
        TyKind::Object { props, index: idx } => {
            if let Expr::String(key) = index {
                let name = types.intern_name(key);
                if let Some((ty, opt)) = object_property(types, stripped, name) {
                    return Some(if opt {
                        types.with_absences(ty, Absence::Undefined)
                    } else {
                        ty
                    });
                }
            }
            if let Some(idx_ty) = idx {
                return Some(types.with_absences(idx_ty, Absence::Undefined));
            }
            let items: Vec<Ty> = types.object_slice(props).iter().map(|p| p.ty).collect();
            (!items.is_empty()).then(|| types.union(&items))
        }
        TyKind::Generic { .. } => iterated_element(ctx, types, stripped).or_else(|| {
            let hir = types.materialize_hir(ctx, stripped, &|_| None);
            dict_value_ty(ctx, decls, types, &hir)
        }),
        _ => None,
    }
}

/// Evaluate static `Promise.allSettled`, `Map.groupBy`, `performance.now`, and `Temporal.*` calls.
pub(crate) fn static_global_method<'h>(
    ctx: &Context,
    types: &mut TypeTable,
    class: &str,
    method: &str,
    args: &'h [Expr],
    eval: &mut impl FnMut(&'h Expr, &mut TypeTable) -> Ty,
) -> Option<Ty> {
    let promise_elem =
        |types: &mut TypeTable,
         args: &'h [Expr],
         eval: &mut dyn FnMut(&'h Expr, &mut TypeTable) -> Ty| {
            let [items] = args else {
                return Ok(Ty::OPEN);
            };
            if matches!(items, Expr::Array(elems, _) if elems.is_empty()) {
                return Ok(Ty::VOID);
            }
            let src = eval(items, types);
            if src == Ty::BOTTOM {
                return Err(Ty::BOTTOM);
            }
            let elem = iterated_element(ctx, types, src).unwrap_or(Ty::OPEN);
            let unwrapped = unwrap_promise_members(types, elem);
            Ok(if unwrapped == Ty::NEVER {
                Ty::VOID
            } else {
                unwrapped
            })
        };
    match (class, method) {
        ("performance", "now") => Some(Ty::NUMBER),
        ("Promise", "allSettled" | "all" | "race") => {
            let val = match promise_elem(types, args, eval) {
                Ok(v) => v,
                Err(b) => return Some(b),
            };
            let inner = match method {
                "allSettled" => {
                    let base = shapes::TypeId::from_owned(ctx, HirType::Named(SETTLED.to_owned()));
                    let settled = types.generic(base, &[val]);
                    types.array(settled)
                }
                "all" => types.array(val),
                _ => val,
            };
            Some(types.promise(inner))
        }
        ("Promise", "resolve") => {
            let val = args.first().map_or(Ty::VOID, |a| eval(a, types));
            Some(match types.kind(val) {
                TyKind::Promise(_) => val,
                _ => types.promise(val),
            })
        }
        ("Promise", "reject") => Some(types.promise(Ty::NEVER)),
        _ => None,
    }
}

fn unwrap_promise_members(types: &mut TypeTable, elem: Ty) -> Ty {
    match types.kind(elem) {
        TyKind::Promise(inner) => inner,
        TyKind::Union(span) => {
            let members: Vec<Ty> = types.union_slice(span).to_vec();
            let mut out = Vec::with_capacity(members.len());
            for member in members {
                out.push(match types.kind(member) {
                    TyKind::Promise(inner) => inner,
                    _ => member,
                });
            }
            types.union(&out)
        }
        _ => elem,
    }
}

pub(crate) fn temporal_static_method(
    ctx: &Context,
    types: &mut TypeTable,
    class: &str,
    method: &str,
) -> Option<Ty> {
    Some(match (class, method) {
        ("Instant", "from" | "fromEpochMilliseconds") | ("Now", "instant") => {
            types.named(ctx, "Temporal.Instant")
        }
        ("PlainDate", "from") | ("Now", "plainDateISO") => types.named(ctx, "Temporal.PlainDate"),
        ("PlainDate", "compare") => Ty::NUMBER,
        ("PlainDateTime", "from") | ("Now", "plainDateTimeISO") => {
            types.named(ctx, "Temporal.PlainDateTime")
        }
        ("Now", "timeZoneId") => Ty::STRING,
        _ => return None,
    })
}

/// Evaluate `LogicalOp` (`??`, `||`, `&&`) over `Ty` operands.
pub(crate) fn logical_result(types: &mut TypeTable, op: LogicalOp, left: Ty, right: Ty) -> Ty {
    match op {
        LogicalOp::Coalesce | LogicalOp::Or => {
            let (left_inner, _) = types.strip_nullish(left);
            let (right_inner, right_abs) = types.strip_nullish(right);
            let base = if !types.is_open(left_inner) && left_inner != Ty::VOID {
                if !types.is_open(right_inner)
                    && right_inner != Ty::VOID
                    && right_inner != left_inner
                {
                    types.join(left_inner, right_inner)
                } else {
                    left_inner
                }
            } else {
                right_inner
            };
            types.with_absences(base, right_abs)
        }
        LogicalOp::And => {
            let (left_inner, left_abs) = types.strip_nullish(left);
            let (right_inner, right_abs) = types.strip_nullish(right);
            let abs = Absence::join_optional(left_abs, right_abs);
            let base = if !types.is_open(right_inner) && right_inner != Ty::VOID {
                right_inner
            } else {
                left_inner
            };
            types.with_absences(base, abs)
        }
    }
}
