//! Built-in expression, method, property, and constructor typing over dense `Ty`.
use super::types::{ObjectProp, Ty, TyKind, TypeTable};
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

fn is_array_like_ty(types: &TypeTable, ty: Ty) -> bool {
    matches!(types.kind(ty), TyKind::Array(_) | TyKind::Tuple(_))
        || matches!(types.kind(ty), TyKind::Cell { kind, .. } if kind.is_array())
}

pub(crate) fn receiver_of(ctx: &Context, types: &TypeTable, ty: Ty) -> Option<Receiver> {
    let for_name = |name: &str| match name {
        "Map" | "ReadonlyMap" => Some(Receiver::Map),
        "Set" | "ReadonlySet" => Some(Receiver::Set),
        "Stats" | "fs.Stats" => Some(Receiver::Value(Kind::Stats)),
        other => Some(Receiver::Value(Kind::builtin(other)?)),
    };
    match types.kind(ty) {
        _ if is_array_like_ty(types, ty) => Some(Receiver::Value(Kind::Array)),
        TyKind::Union(span)
            if !types.ty_slice(span).is_empty()
                && types
                    .ty_slice(span)
                    .iter()
                    .all(|&m| is_array_like_ty(types, m)) =>
        {
            Some(Receiver::Value(Kind::Array))
        }
        TyKind::String => Some(Receiver::Value(Kind::String)),
        TyKind::Number | TyKind::Int32 => Some(Receiver::Value(Kind::Number)),
        TyKind::Boolean => Some(Receiver::Value(Kind::Boolean)),
        TyKind::BigInt => Some(Receiver::Value(Kind::BigInt)),
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::StringLiteral(_) => Some(Receiver::Value(Kind::String)),
            HirType::Named(name) => for_name(name),
            _ => None,
        },
        TyKind::Generic { base, .. } => match base.hir(ctx) {
            HirType::Named(name) => for_name(name),
            _ => None,
        },
        _ => None,
    }
}

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
    if property || matches!(types.kind(receiver), TyKind::ClassInst(_)) {
        return None;
    }
    Method::inferred(
        &types.materialize_hir(ctx, receiver, &|_| None),
        name,
        false,
    )
}

pub(crate) fn iterated_element(ctx: &Context, types: &mut TypeTable, ty: Ty) -> Option<Ty> {
    let (ty, _) = types.strip_nullish(ty);
    match types.kind(ty) {
        TyKind::String => Some(Ty::STRING),
        TyKind::Array(elem) => Some(elem),
        TyKind::Tuple(span) => {
            let items = types.ty_slice(span).to_vec();
            Some(types.union(&items))
        }
        TyKind::Union(span) => {
            let mut elems = Vec::new();
            for m in types.ty_slice(span).to_vec() {
                elems.push(iterated_element(ctx, types, m)?);
            }
            (!elems.is_empty()).then(|| types.union(&elems))
        }
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::StringLiteral(_) => Some(Ty::STRING),
            HirType::Named(n) if n == "Uint8Array" => Some(Ty::NUMBER),
            _ => None,
        },
        TyKind::Generic { base, args } => {
            let HirType::Named(name) = base.hir(ctx) else {
                return None;
            };
            match (name.as_str(), types.ty_slice(args).to_vec().as_slice()) {
                ("Set" | "ReadonlySet" | "SetValues" | "Array" | "ReadonlyArray", [e]) => Some(*e),
                ("MapKeys", [k, _]) => Some(*k),
                ("MapValues", [_, v]) => Some(*v),
                ("Map" | "ReadonlyMap" | "MapEntries", [k, v]) => Some(types.tuple(&[*k, *v])),
                ("SetEntries", [e]) => Some(types.tuple(&[*e, *e])),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn iterator_view(
    ctx: &Context,
    types: &mut TypeTable,
    source: Ty,
    method: IteratorMethod,
) -> Option<Ty> {
    let (source, _) = types.strip_nullish(source);
    match types.kind(source) {
        TyKind::Array(elem) => {
            let out = match method {
                IteratorMethod::Keys => Ty::NUMBER,
                IteratorMethod::Values => elem,
                IteratorMethod::Entries => types.tuple(&[Ty::NUMBER, elem]),
            };
            Some(types.array(out))
        }
        TyKind::Generic { base, args } => {
            let HirType::Named(name) = base.hir(ctx) else {
                return None;
            };
            let slice = types.ty_slice(args).to_vec();
            let (view_name, kept): (&str, &[Ty]) = match (name.as_str(), slice.as_slice()) {
                ("Map" | "ReadonlyMap", [_, _]) => (method.map_view().type_name(), &slice),
                ("Set" | "ReadonlySet", [e, ..]) => {
                    (method.set_view().type_name(), std::slice::from_ref(e))
                }
                _ => return None,
            };
            Some(types.generic(
                shapes::TypeId::from_owned(ctx, HirType::Named(view_name.to_owned())),
                kept,
            ))
        }
        _ => None,
    }
}

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
            Some(Ty::BOTTOM) => Ty::BOTTOM,
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
            Some(Ty::BOTTOM) => Ty::BOTTOM,
            Some(ret) if !types.is_open(ret) => {
                if matches!(types.kind(ret), TyKind::Array(_)) {
                    ret
                } else {
                    types.array(ret)
                }
            }
            _ => Ty::OPEN,
        },
        MethodResult::Receiver => receiver,
        MethodResult::Flat => {
            let zero = matches!(arg0_expr, Some(Expr::Number(n)) if *n <= 0.0)
                || matches!(arg0_expr, Some(Expr::Integer(n)) if *n <= 0);
            let (stripped, _) = types.strip_nullish(receiver);
            match types.kind(stripped) {
                TyKind::Array(inner) if !zero => {
                    let (def, states) = types.strip_nullish(inner);
                    let elem = match types.kind(def) {
                        TyKind::Array(e) => e,
                        _ => def,
                    };
                    let with_states = types.with_absences(elem, states);
                    types.array(with_states)
                }
                _ => stripped,
            }
        }
        MethodResult::Element | MethodResult::At => {
            let (stripped, _) = types.strip_nullish(receiver);
            if rule == MethodResult::At && stripped == Ty::STRING {
                return types.with_absences(Ty::STRING, Absence::Undefined);
            }
            iterated_element(ctx, types, stripped)
                .map_or(Ty::OPEN, |e| types.with_absences(e, Absence::Undefined))
        }
        MethodResult::Reduced => {
            if callback_result == Some(Ty::BOTTOM) {
                return Ty::BOTTOM;
            }
            if let Some(ret) = callback_result
                && !types.is_open(ret)
                && ret != Ty::VOID
            {
                return ret;
            }
            if let Some(init) = arg0
                && !types.is_open(init)
            {
                return init;
            }
            let stripped = types.strip_nullish(receiver).0;
            match types.kind(stripped) {
                TyKind::Array(elem) => elem,
                _ => Ty::OPEN,
            }
        }
        MethodResult::Void => Ty::VOID,
        MethodResult::String => Ty::STRING,
        MethodResult::Strings => types.array(Ty::STRING),
        MethodResult::NullableStrings => {
            let opt = types.with_absences(Ty::STRING, Absence::Undefined);
            let arr = types.array(opt);
            types.with_absences(arr, Absence::Null)
        }
        MethodResult::Number => Ty::NUMBER,
        MethodResult::Boolean => Ty::BOOLEAN,
        MethodResult::OptionalNumber => types.with_absences(Ty::NUMBER, Absence::Undefined),
        MethodResult::NullableString => types.with_absences(Ty::STRING, Absence::Null),
        MethodResult::Native(k) => native_ty(ctx, types, k),
        MethodResult::Nullable(k) => {
            let t = native_ty(ctx, types, k);
            types.with_absences(t, Absence::Null)
        }
        MethodResult::Optional(k) => {
            let t = native_ty(ctx, types, k);
            types.with_absences(t, Absence::Undefined)
        }
        MethodResult::Promise(k) => {
            let t = native_ty(ctx, types, k);
            types.promise(t)
        }
        MethodResult::PromiseVoid => types.promise(Ty::VOID),
        MethodResult::PromiseJson => types.promise(Ty::DYNAMIC),
        MethodResult::StreamReader => {
            let byob = arg0_expr.is_some_and(|arg| match arg {
                Expr::Object(p) => p
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
                if byob { Kind::ByobReader } else { Kind::Reader },
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
                && let HirType::Named(name) = base.hir(ctx)
                && matches!(name.as_str(), "Map" | "ReadonlyMap")
                && let [_, val] = types.ty_slice(args)
            {
                let v = *val;
                types.with_absences(v, Absence::Undefined)
            } else {
                Ty::OPEN
            }
        }
        MethodResult::Duration => {
            let props = [
                ("blank", Ty::BOOLEAN),
                ("days", Ty::NUMBER),
                ("months", Ty::NUMBER),
                ("sign", Ty::NUMBER),
                ("weeks", Ty::NUMBER),
                ("years", Ty::NUMBER),
            ]
            .map(|(n, ty)| ObjectProp {
                name: types.intern_name(n),
                ty,
                optional: false,
                readonly: false,
            });
            types.object(&props, None)
        }
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

pub(crate) fn binary_result(
    ctx: &Context,
    types: &mut TypeTable,
    op: BinaryOp,
    left: Ty,
    right: Ty,
) -> Ty {
    let is_str = |t: Ty| {
        matches!(types.kind(t), TyKind::String)
            || matches!(types.kind(t), TyKind::Leaf(id) if matches!(id.hir(ctx), HirType::StringLiteral(_)))
    };
    let is_mixed_str = |t: Ty| {
        matches!(types.kind(t), TyKind::Union(span) if types.ty_slice(span).iter().any(|&m| {
            !matches!(m, Ty::NUMBER | Ty::INT32 | Ty::BOOLEAN | Ty::NULL | Ty::VOID)
                && (is_str(m) || !matches!(types.kind(m), TyKind::Bottom | TyKind::Open))
        }))
    };
    if op == BinaryOp::Add {
        if is_str(left) || is_str(right) {
            return Ty::STRING;
        }
        if left == Ty::BOTTOM || right == Ty::BOTTOM {
            return Ty::BOTTOM;
        }
        if left == Ty::BIGINT && right == Ty::BIGINT {
            return Ty::BIGINT;
        }
        if is_mixed_str(left) || is_mixed_str(right) {
            return types.union(&[Ty::NUMBER, Ty::STRING]);
        }
        return Ty::NUMBER;
    }
    if left == Ty::BIGINT && right == Ty::BIGINT {
        Ty::BIGINT
    } else {
        Ty::NUMBER
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
        Expr::Uint8ArrayGet { .. } | Expr::BufferIndexGet { .. } => {
            types.with_absences(Ty::NUMBER, Absence::Undefined)
        }
        Expr::StringAt { .. } => types.with_absences(Ty::STRING, Absence::Undefined),
        Expr::DateToJSON(_) => types.with_absences(Ty::STRING, Absence::Null),
        Expr::JsonParse(_) | Expr::JsonParseWithReviver(..) | Expr::JsonParseReviver { .. } => {
            Ty::DYNAMIC
        }
        Expr::JsonParseTyped { ty, .. } => {
            let p = types.intern_hir(ctx, ty, true);
            if p == Ty::OPEN { Ty::DYNAMIC } else { p }
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
                let opt = types.with_absences(Ty::STRING, Absence::Undefined);
                let rec = types.object(&[], Some(opt));
                Some(types.with_absences(rec, Absence::Undefined))
            }
            _ => None,
        },
        TyKind::Cell { kind, .. } if kind.is_array() => {
            matches!(property, "length" | "index").then_some(Ty::NUMBER)
        }
        TyKind::Object { props, index } => {
            let target = types.intern_name(property);
            if let Some(p) = types
                .object_slice(props)
                .iter()
                .find(|p| p.name == target)
                .copied()
            {
                return Some(if p.optional {
                    types.with_absences(p.ty, Absence::Undefined)
                } else {
                    p.ty
                });
            }
            index.map(|idx| types.with_absences(idx, Absence::Undefined))
        }
        TyKind::Leaf(id) | TyKind::Generic { base: id, .. } => {
            let HirType::Named(base_name) = id.hir(ctx) else {
                return (matches!(id.hir(ctx), HirType::StringLiteral(_)) && property == "length")
                    .then_some(Ty::NUMBER);
            };
            let gargs = match types.kind(stripped) {
                TyKind::Generic { args, .. } => Some(args),
                _ => None,
            };
            if let Some(args) = gargs {
                if matches!(
                    base_name.as_str(),
                    "Map" | "ReadonlyMap" | "Set" | "ReadonlySet"
                ) && property == "size"
                {
                    return Some(Ty::NUMBER);
                }
                if base_name == SETTLED && !decls.is_user_type(base_name) {
                    return match (property, types.ty_slice(args).to_vec().as_slice()) {
                        ("status", _) => Some(Ty::STRING),
                        ("value", [v]) => Some(types.with_absences(*v, Absence::Undefined)),
                        ("reason", _) => {
                            let e = types.named(ctx, "Error");
                            Some(types.with_absences(e, Absence::Undefined))
                        }
                        _ => None,
                    };
                }
            } else {
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
            }
            let arg_hirs: Vec<HirType> = gargs
                .map(|a| {
                    types
                        .ty_slice(a)
                        .iter()
                        .map(|&t| types.materialize_hir(ctx, t, &|_| None))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(prop_hir) = decls.property_type(ctx, base_name, &arg_hirs, property) {
                let ty = types.intern_hir(ctx, &prop_hir, true);
                if let TyKind::Leaf(lid) = types.kind(ty)
                    && let HirType::Named(n) = lid.hir(ctx)
                    && let Some(carrier) = decls.integer_carrier(n)
                {
                    return Some(if matches!(carrier, HirType::BigInt) {
                        Ty::BIGINT
                    } else {
                        Ty::NUMBER
                    });
                }
                return Some(ty);
            }
            if gargs.is_none()
                && let Some(resolved) = decls.resolve_foreign_type(base_name)
            {
                let rty = types.intern_hir(ctx, resolved, true);
                if rty != stripped
                    && let Some(found) = property_type(ctx, decls, types, rty, property)
                {
                    return Some(found);
                }
            }
            let full_hir = if gargs.is_some() {
                HirType::Generic {
                    base: base_name.clone(),
                    type_args: arg_hirs,
                }
            } else {
                HirType::Named(base_name.clone())
            };
            dict_value_ty(ctx, decls, types, &full_hir).or_else(|| {
                gargs
                    .is_none()
                    .then(|| Method::named_property(base_name, property))
                    .flatten()
                    .or_else(|| lookup_method_result(ctx, types, stripped, property, true))
                    .map(|rule| {
                        apply_method_result(
                            ctx,
                            types,
                            rule,
                            MethodResultArgs::receiver_only(stripped),
                        )
                    })
            })
        }
        TyKind::Union(span) => {
            let res: Vec<Ty> = types
                .ty_slice(span)
                .to_vec()
                .into_iter()
                .filter_map(|m| property_type(ctx, decls, types, m, property))
                .collect();
            (!res.is_empty()).then(|| types.union(&res))
        }
        _ => None,
    }
}

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
        TyKind::Array(elem) => Some(elem),
        TyKind::String => Some(Ty::STRING),
        TyKind::Tuple(span) => {
            let pos = match index {
                Expr::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
                Expr::Integer(n) => usize::try_from(*n).ok(),
                _ => None,
            };
            let slice = types.ty_slice(span);
            match pos {
                Some(i) => Some(slice.get(i).copied().unwrap_or(Ty::VOID)),
                None => {
                    let items = slice.to_vec();
                    Some(types.union(&items))
                }
            }
        }
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::Named(n) if n == "Uint8Array" => {
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
                if let Some(p) = types
                    .object_slice(props)
                    .iter()
                    .find(|p| p.name == name)
                    .copied()
                {
                    return Some(if p.optional {
                        types.with_absences(p.ty, Absence::Undefined)
                    } else {
                        p.ty
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

pub(crate) fn static_global_method(
    ctx: &Context,
    types: &mut TypeTable,
    class: &str,
    method: &str,
    args: &[Expr],
    arg_tys: &[Ty],
) -> Option<Ty> {
    match (class, method) {
        ("performance", "now") => Some(Ty::NUMBER),
        ("Promise", "allSettled" | "all" | "race") => {
            let val = match args {
                [Expr::Array(elems, _)] if elems.is_empty() => Ty::VOID,
                [_] => {
                    let src = arg_tys.first().copied().unwrap_or(Ty::OPEN);
                    if src == Ty::BOTTOM {
                        return Some(Ty::BOTTOM);
                    }
                    let elem = iterated_element(ctx, types, src).unwrap_or(Ty::OPEN);
                    let unwrap = |t: Ty, types: &TypeTable| match types.kind(t) {
                        TyKind::Promise(i) => i,
                        _ => t,
                    };
                    let unwrapped = match types.kind(elem) {
                        TyKind::Union(span) => {
                            let out: Vec<Ty> = types
                                .ty_slice(span)
                                .iter()
                                .map(|&m| unwrap(m, types))
                                .collect();
                            types.union(&out)
                        }
                        _ => unwrap(elem, types),
                    };
                    if unwrapped == Ty::NEVER {
                        Ty::VOID
                    } else {
                        unwrapped
                    }
                }
                _ => Ty::OPEN,
            };
            let inner = match method {
                "allSettled" => {
                    let settled = types.generic(
                        shapes::TypeId::from_owned(ctx, HirType::Named(SETTLED.to_owned())),
                        &[val],
                    );
                    types.array(settled)
                }
                "all" => types.array(val),
                _ => val,
            };
            Some(types.promise(inner))
        }
        ("Promise", "resolve") => {
            let val = arg_tys.first().copied().unwrap_or(Ty::VOID);
            Some(if matches!(types.kind(val), TyKind::Promise(_)) {
                val
            } else {
                types.promise(val)
            })
        }
        ("Promise", "reject") => Some(types.promise(Ty::NEVER)),
        _ => None,
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

pub(crate) fn logical_result(types: &mut TypeTable, op: LogicalOp, left: Ty, right: Ty) -> Ty {
    let (l_in, l_abs) = types.strip_nullish(left);
    let (r_in, r_abs) = types.strip_nullish(right);
    match op {
        LogicalOp::Coalesce => match l_abs {
            None => left,
            Some(_) if l_in == Ty::VOID => right,
            Some(_) => {
                let j = types.join(l_in, r_in);
                types.with_absences(j, r_abs)
            }
        },
        LogicalOp::Or => {
            let j = types.join(l_in, r_in);
            types.with_absences(j, r_abs)
        }
        LogicalOp::And => {
            let j = types.join(left, right);
            types.with_absences(j, Absence::join_optional(l_abs, r_abs))
        }
    }
}
