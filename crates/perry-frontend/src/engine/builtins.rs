//! Built-in expression, method, property, and constructor typing over dense `Ty`.
use super::types::{Ty, TyKind, TypeTable};
use crate::engine::types::WitView;
use crate::{
    Absence,
    collections::{CollectionViewKind, IteratorMethod},
    methods::{Kind, Method, MethodResult, Receiver},
    shapes::{self, Declarations},
    source::CollectionKind,
};
use perry_hir::ir::{BinaryOp, Expr, LogicalOp, UnaryOp};
use pliron::context::Context;

pub(crate) const SETTLED: &str = "PromiseSettledResult";

fn dense_array(types: &TypeTable, ty: Ty) -> bool {
    matches!(types.kind(ty), TyKind::Array(_))
        || matches!(types.kind(ty), TyKind::Cell { kind, .. } if kind.is_array())
}

pub(crate) fn receiver_of(
    ctx: &Context,
    decls: &Declarations,
    types: &TypeTable,
    ty: Ty,
) -> Option<Receiver> {
    match types.kind(ty) {
        _ if dense_array(types, ty) => Some(Receiver::Value(Kind::Array)),
        TyKind::Union(span)
            if !types.ty_slice(span).is_empty()
                && types.ty_slice(span).iter().all(|&m| dense_array(types, m)) =>
        {
            Some(Receiver::Value(Kind::Array))
        }
        TyKind::Regex(_) => Some(Receiver::Value(Kind::Regex)),
        TyKind::String => Some(Receiver::Value(Kind::String)),
        TyKind::Number | TyKind::Int32 => Some(Receiver::Value(Kind::Number)),
        TyKind::Integer(domain) => Some(Receiver::Value(if domain.deref(ctx).width() == 64 {
            Kind::BigInt
        } else {
            Kind::Number
        })),
        TyKind::Boolean => Some(Receiver::Value(Kind::Boolean)),
        TyKind::BigInt => Some(Receiver::Value(Kind::BigInt)),
        TyKind::Literal(_) => Some(Receiver::Value(Kind::String)),
        TyKind::Named(id) | TyKind::Generic { base: id, .. }
            if !decls.is_user_type(id.get(ctx))
                && !decls.imported_types.contains_key(id.get(ctx).as_str()) =>
        {
            let name = id.get(ctx);
            CollectionKind::from_type_name(name)
                .map(Receiver::Collection)
                .or_else(|| Kind::builtin(name).map(Receiver::Value))
        }
        _ => None,
    }
}

pub(crate) fn iterated_element(ctx: &Context, types: &mut TypeTable, ty: Ty) -> Option<Ty> {
    let (ty, _) = types.strip_nullish(ty);
    match types.kind(ty) {
        TyKind::String => Some(Ty::STRING),
        TyKind::Array(elem) => Some(elem),
        TyKind::Tuple(span) => Some(types.union_span(span)),
        TyKind::Union(span) => {
            let mut elems = Vec::new();
            for m in types.ty_slice(span).to_vec() {
                elems.push(iterated_element(ctx, types, m)?);
            }
            (!elems.is_empty()).then(|| types.union(&elems))
        }
        TyKind::Literal(_) => Some(Ty::STRING),
        TyKind::Named(id) if id.get(ctx) == "Uint8Array" => Some(Ty::NUMBER),
        TyKind::Generic { base, args } => {
            let name = base.get(ctx);
            let arguments = types.ty_slice(args);
            if matches!(name.as_str(), "Array" | "ReadonlyArray") {
                return match arguments {
                    &[element] => Some(element),
                    _ => None,
                };
            }
            let view = CollectionViewKind::from_type_name(name).or_else(|| {
                CollectionKind::from_type_name(name).map(CollectionKind::default_view)
            })?;
            let fields = view.source().type_fields(arguments)?;
            Some(
                view.project(fields)
                    .materialize(std::convert::identity, |fields| types.tuple(&fields)),
            )
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
            let out = method
                .project([Ty::NUMBER, elem])
                .materialize(std::convert::identity, |fields| types.tuple(&fields));
            Some(types.array(out))
        }
        TyKind::Generic { base, args } => {
            let kind = CollectionKind::from_type_name(base.get(ctx))?;
            let name = shapes::TypeName::new(ctx, method.view(kind).type_name().into());
            match (kind, types.ty_slice(args)) {
                (CollectionKind::Map, &[k, v]) => Some(types.generic(name, &[k, v])),
                (CollectionKind::Set, &[v, ..]) => Some(types.generic(name, &[v])),
                _ => None,
            }
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
impl<'a> MethodResultArgs<'a> {
    pub(super) fn call(receiver: Ty, args: &'a [Expr], types: &[Ty], sparse_arrays: bool) -> Self {
        Self {
            arg0: types.first().copied(),
            arg0_expr: args.first(),
            sparse_arrays,
            ..Self::receiver_only(receiver)
        }
    }

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
        MethodResult::Signature(kinds) => signature_result(ctx, types, kinds),
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
        MethodResult::Iterator(method) => {
            iterator_view(ctx, types, receiver, method).unwrap_or(Ty::OPEN)
        }
        MethodResult::Lookup => {
            let (stripped, _) = types.strip_nullish(receiver);
            if let TyKind::Generic { base, args } = types.kind(stripped)
                && matches!(base.get(ctx).as_str(), "Map" | "ReadonlyMap")
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
            .map(|(name, ty)| types.property(name, ty, false));
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

pub(crate) fn binary_result(types: &mut TypeTable, op: BinaryOp, left: Ty, right: Ty) -> Ty {
    let is_str = |t: Ty| {
        matches!(types.kind(t), TyKind::String) || matches!(types.kind(t), TyKind::Literal(_))
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

pub(crate) enum IntrinsicInput<'h> {
    Number(&'h Expr),
    CoerceNumber(&'h Expr),
    Display(&'h Expr),
    Required(&'h Expr, Kind, &'static str),
    Literal(&'h str),
}

pub(crate) fn hir_intrinsic(
    expr: &Expr,
) -> Option<(
    perry_mir::Intrinsic,
    impl Iterator<Item = IntrinsicInput<'_>>,
)> {
    use IntrinsicInput::*;
    use perry_mir::Intrinsic;
    let one = |a| [Some(a), None];
    let two = |a, b| [Some(a), Some(b)];
    let (op, inputs) = match expr {
        Expr::MathFloor(value) => (Intrinsic::MathFloor, one(Number(value))),
        Expr::MathCeil(value) => (Intrinsic::MathCeil, one(Number(value))),
        Expr::MathTrunc(value) => (Intrinsic::MathTrunc, one(Number(value))),
        Expr::MathAbs(value) => (Intrinsic::MathAbs, one(Number(value))),
        Expr::MathRound(value) => (Intrinsic::MathRound, one(Number(value))),
        Expr::MathSqrt(value) => (Intrinsic::MathSqrt, one(Number(value))),
        Expr::MathImul(a, b) => (Intrinsic::MathImul, two(Number(a), Number(b))),
        Expr::IsNaN(value) => (Intrinsic::NumberIsNaN, one(CoerceNumber(value))),
        Expr::IsFinite(value) => (Intrinsic::NumberIsFinite, one(CoerceNumber(value))),
        Expr::NumberIsNaN(value) => (Intrinsic::NumberIsNaN, one(Number(value))),
        Expr::NumberIsFinite(value) => (Intrinsic::NumberIsFinite, one(Number(value))),
        Expr::NumberIsInteger(value) => (Intrinsic::NumberIsInteger, one(Number(value))),
        Expr::NumberIsSafeInteger(value) => (Intrinsic::NumberIsSafeInteger, one(Number(value))),
        Expr::MathRandom => (Intrinsic::MathRandom, [None, None]),
        Expr::DateNow => (Intrinsic::DateNow, [None, None]),
        Expr::StringFromCodePoint(value) => (Intrinsic::StringFromCodePoint, one(Number(value))),
        Expr::DateParse(value) => (
            Intrinsic::DateParse,
            one(Required(value, Kind::String, "a string")),
        ),
        Expr::ParseFloat(value) => (Intrinsic::ParseFloat, one(Display(value))),
        Expr::EncodeURIComponent(value) => (
            Intrinsic::EncodeUriComponent,
            one(Required(value, Kind::String, "a string")),
        ),
        Expr::EnvGet(name) => (Intrinsic::EnvGet, one(Literal(name))),
        Expr::UrlGetSearchParams(value) => (
            Intrinsic::UrlSearchParams,
            one(Required(value, Kind::Url, "a URL")),
        ),
        Expr::TextDecoderFatal(value) => (
            Intrinsic::DecoderFatal,
            one(Required(value, Kind::Decoder, "a TextDecoder")),
        ),
        Expr::TextDecoderIgnoreBom(value) => (
            Intrinsic::DecoderIgnoreBom,
            one(Required(value, Kind::Decoder, "a TextDecoder")),
        ),
        Expr::Uint8ArrayGet { array, index } => (
            Intrinsic::BytesGet,
            two(Required(array, Kind::Bytes, "a Uint8Array"), Number(index)),
        ),
        Expr::CryptoRandomUUID => (Intrinsic::RandomUuid, [None, None]),
        Expr::TextEncoderNew => (Intrinsic::EncoderNew, [None, None]),
        _ => return None,
    };
    Some((op, inputs.into_iter().flatten()))
}

pub(super) fn signature_result(
    ctx: &Context,
    types: &mut TypeTable,
    kinds: &[perry_mir::SignatureType],
) -> Ty {
    use perry_mir::SignatureType;
    match kinds {
        [] => Ty::VOID,
        [SignatureType::PromiseVoid] => types.promise(Ty::VOID),
        [SignatureType::Incoming] => types.named(ctx, "__PerryIncoming"),
        [kind] => signature_value(ctx, types, kind.canonical(ctx)),
        _ => Ty::OPEN,
    }
}

fn signature_value(ctx: &Context, types: &mut TypeTable, value: pliron::r#type::TypeHandle) -> Ty {
    let present = perry_mir::present_type(ctx, value);
    let ty = if let Some(array) = perry_mir::typed::<perry_mir::ArrayType>(ctx, present) {
        let element = signature_value(ctx, types, array.deref(ctx).element());
        types.array(element)
    } else if let Some(promise) = perry_mir::typed::<perry_mir::PromiseType>(ctx, present) {
        let settled = signature_value(ctx, types, promise.deref(ctx).value());
        types.promise(settled)
    } else {
        match Kind::of(ctx, present) {
            Some(Kind::Json) => Ty::DYNAMIC,
            Some(kind) => native_ty(ctx, types, kind),
            None => Ty::OPEN,
        }
    };
    types.with_absences(ty, perry_mir::absence(ctx, value))
}

pub(crate) fn leaf_expr_type(ctx: &Context, types: &mut TypeTable, expr: &Expr) -> Ty {
    if let Some((op, _)) = hir_intrinsic(expr) {
        return signature_result(ctx, types, op.result_kinds());
    }
    match expr {
        Expr::JsonParse(_) | Expr::JsonParseWithReviver(..) => Ty::DYNAMIC,
        Expr::JsonParseTyped { ty, .. } => {
            let p = types.intern_hir(ctx, ty, true);
            if p == Ty::OPEN { Ty::DYNAMIC } else { p }
        }
        Expr::Uint8ArraySet { .. } => Ty::NUMBER,
        Expr::UrlSetPathname { .. } => Ty::STRING,
        _ => {
            use perry_hir::types::Type;
            let ty = perry_hir::analysis::infer_expr_type(expr, &());
            match &ty {
                Type::Boolean
                | Type::Number
                | Type::BigInt
                | Type::String
                | Type::Null
                | Type::Never
                | Type::Void => types.intern_hir(ctx, &ty, false),
                Type::Named(name) if Kind::builtin(name).is_some() => types.named(ctx, name),
                Type::Array(element) if **element == Type::String => types.array(Ty::STRING),
                _ => Ty::OPEN,
            }
        }
    }
}

fn dict_value_ty(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &mut TypeTable,
    ty: Ty,
) -> Option<Ty> {
    let (_, val_ty) = crate::shapes::generics::Resolver::new(ctx, decls, types).dictionary(ty)?;
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
        TyKind::Literal(_) => (property == "length").then_some(Ty::NUMBER),
        TyKind::Named(id) | TyKind::Generic { base: id, .. } => {
            let base_name = id.get(ctx);
            let gargs = match types.kind(stripped) {
                TyKind::Generic { args, .. } => Some(args),
                _ => None,
            };
            if let Some(args) = gargs {
                if base_name == "WitInput"
                    && let &[inner] = types.ty_slice(args)
                {
                    let field = property_type(ctx, decls, types, inner, property)?;
                    return Some(types.wit_input(ctx, field));
                }
                if property == "size" && CollectionKind::from_type_name(base_name).is_some() {
                    return Some(Ty::NUMBER);
                }
                if base_name == SETTLED && !decls.is_user_type(base_name) {
                    let arg0 = match types.ty_slice(args) {
                        &[v] => Some(v),
                        _ => None,
                    };
                    return match (property, arg0) {
                        ("status", _) => Some(Ty::STRING),
                        ("value", Some(v)) => Some(types.with_absences(v, Absence::Undefined)),
                        ("reason", _) => {
                            let e = types.named(ctx, "Error");
                            Some(types.with_absences(e, Absence::Undefined))
                        }
                        _ => None,
                    };
                }
            }
            let arguments = gargs
                .map(|a| types.ty_slice(a).to_vec())
                .unwrap_or_default();
            if let Some((ty, optional)) = crate::shapes::generics::Resolver::new(ctx, decls, types)
                .property(base_name, &arguments, property)
            {
                let ty = if optional {
                    types.with_absences(ty, Absence::Undefined)
                } else {
                    ty
                };
                if let Some(domain) = types.integer_domain(ctx, decls, ty) {
                    return Some(if domain.deref(ctx).width() == 64 {
                        Ty::BIGINT
                    } else {
                        Ty::NUMBER
                    });
                }
                return Some(ty);
            }
            if gargs.is_none()
                && let Some(resolved) = decls.foreign_type(base_name)
            {
                let rty = types.wit_type(ctx, resolved, WitView::Definition);
                if rty != stripped
                    && let Some(found) = property_type(ctx, decls, types, rty, property)
                {
                    return Some(found);
                }
            }
            dict_value_ty(ctx, decls, types, stripped).or_else(|| {
                receiver_of(ctx, decls, types, stripped)
                    .and_then(|r| Method::lookup(r, property))
                    .filter(|m| m.is_property())
                    .map(|m| {
                        apply_method_result(
                            ctx,
                            types,
                            m.result_rule(),
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
        TyKind::Tuple(span) => match crate::hir_compat::tuple_position(index) {
            Some(i) => Some(types.ty_slice(span).get(i).copied().unwrap_or(Ty::VOID)),
            None => Some(types.union_span(span)),
        },
        TyKind::Named(id) if id.get(ctx) == "Uint8Array" => {
            Some(types.with_absences(Ty::NUMBER, Absence::Undefined))
        }
        TyKind::Named(id) => dict_value_ty(ctx, decls, types, stripped).or_else(|| {
            let ty = types.wit_type(ctx, decls.foreign_type(id.get(ctx))?, WitView::Definition);
            match types.kind(ty) {
                TyKind::Array(elem) => Some(elem),
                _ => None,
            }
        }),
        TyKind::Object { props, index: idx } => {
            if let Some(idx_ty) = idx {
                return Some(types.with_absences(idx_ty, Absence::Undefined));
            }
            let items: Vec<Ty> = types.object_slice(props).iter().map(|p| p.ty).collect();
            (!items.is_empty()).then(|| types.union(&items))
        }
        TyKind::Generic { .. } => iterated_element(ctx, types, stripped)
            .or_else(|| dict_value_ty(ctx, decls, types, stripped)),
        _ => None,
    }
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
