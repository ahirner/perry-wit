//! Lowering for `Map<K, V>`, `Set<T>`, `Record<K, V>`, `Object.*`, `Array.isArray`, and `in`.

use super::*;
use perry_mir::types::OptionalType;
use perry_parser::swc_ecma_ast as ast;
use swc_common::DUMMY_SP;
use swc_ecma_visit::{VisitMut, VisitMutWith};

mod objects;

pub(super) const MAP_TYPE_MARKER: &str = "\0perry_map_type_args";
pub(super) const SET_TYPE_MARKER: &str = "\0perry_set_type_args";

/// Rewrite `new Map<K, V>(...)` and `new Set<T>(...)` in the AST before HIR lowering
/// so explicit constructor type arguments are preserved even when the variable binding
/// has no type annotation.
pub(super) fn preserve_type_args(module: &mut ast::Module) {
    struct TypeArgPreserver;

    impl TypeArgPreserver {
        fn ts_type_to_hir(ty: &ast::TsType) -> Option<HirType> {
            match ty {
                ast::TsType::TsKeywordType(kw) => match kw.kind {
                    ast::TsKeywordTypeKind::TsStringKeyword => Some(HirType::String),
                    ast::TsKeywordTypeKind::TsNumberKeyword => Some(HirType::Number),
                    ast::TsKeywordTypeKind::TsBooleanKeyword => Some(HirType::Boolean),
                    ast::TsKeywordTypeKind::TsBigIntKeyword => Some(HirType::BigInt),
                    ast::TsKeywordTypeKind::TsVoidKeyword
                    | ast::TsKeywordTypeKind::TsUndefinedKeyword => Some(HirType::Void),
                    ast::TsKeywordTypeKind::TsNullKeyword => Some(HirType::Null),
                    ast::TsKeywordTypeKind::TsAnyKeyword => Some(HirType::Any),
                    ast::TsKeywordTypeKind::TsUnknownKeyword => Some(HirType::Unknown),
                    _ => None,
                },
                ast::TsType::TsArrayType(arr) => Some(HirType::Array(Box::new(
                    Self::ts_type_to_hir(&arr.elem_type)?,
                ))),
                ast::TsType::TsTupleType(tuple) => {
                    let mut elems = Vec::with_capacity(tuple.elem_types.len());
                    for elem in &tuple.elem_types {
                        elems.push(Self::ts_type_to_hir(&elem.ty)?);
                    }
                    Some(HirType::Tuple(elems))
                }
                ast::TsType::TsUnionOrIntersectionType(
                    ast::TsUnionOrIntersectionType::TsUnionType(u),
                ) => {
                    let mut items = Vec::with_capacity(u.types.len());
                    for member in &u.types {
                        items.push(Self::ts_type_to_hir(member)?);
                    }
                    Some(HirType::Union(items))
                }
                ast::TsType::TsParenthesizedType(p) => Self::ts_type_to_hir(&p.type_ann),
                ast::TsType::TsTypeRef(type_ref) => {
                    let ast::TsEntityName::Ident(ident) = &type_ref.type_name else {
                        return None;
                    };
                    let name = ident.sym.as_ref();
                    if let Some(params) = &type_ref.type_params {
                        let mut args = Vec::with_capacity(params.params.len());
                        for param in &params.params {
                            args.push(Self::ts_type_to_hir(param)?);
                        }
                        if matches!(name, "Array" | "ReadonlyArray") && args.len() == 1 {
                            return Some(HirType::Array(Box::new(args.swap_remove(0))));
                        }
                        Some(HirType::Generic {
                            base: name.to_string(),
                            type_args: args,
                        })
                    } else {
                        Some(HirType::Named(name.to_string()))
                    }
                }
                ast::TsType::TsLitType(lit) => match &lit.lit {
                    ast::TsLit::Str(s) => Some(HirType::StringLiteral(
                        s.value.to_string_lossy().into_owned(),
                    )),
                    _ => None,
                },
                _ => None,
            }
        }

        fn str_arg(s: &str) -> ast::ExprOrSpread {
            ast::ExprOrSpread {
                spread: None,
                expr: Box::new(ast::Expr::Lit(ast::Lit::Str(ast::Str {
                    span: DUMMY_SP,
                    value: s.into(),
                    raw: None,
                }))),
            }
        }
    }

    impl VisitMut for TypeArgPreserver {
        fn visit_mut_new_expr(&mut self, node: &mut ast::NewExpr) {
            node.visit_mut_children_with(self);
            let ast::Expr::Ident(ident) = node.callee.as_ref() else {
                return;
            };
            let Some(type_args) = node.type_args.as_ref() else {
                return;
            };
            if ident.sym.as_ref() == "Map" && type_args.params.len() == 2 {
                if let (Some(k_hir), Some(v_hir)) = (
                    Self::ts_type_to_hir(&type_args.params[0]),
                    Self::ts_type_to_hir(&type_args.params[1]),
                ) && let (Some(k_enc), Some(v_enc)) =
                    (encode_hir_type(&k_hir), encode_hir_type(&v_hir))
                {
                    let mut elems = vec![
                        Some(Self::str_arg(MAP_TYPE_MARKER)),
                        Some(Self::str_arg(&k_enc)),
                        Some(Self::str_arg(&v_enc)),
                    ];
                    if let Some(first) = node
                        .args
                        .take()
                        .and_then(|mut args| (!args.is_empty()).then(|| args.swap_remove(0)))
                    {
                        elems.push(Some(first));
                    }
                    node.args = Some(vec![ast::ExprOrSpread {
                        spread: None,
                        expr: Box::new(ast::Expr::Array(ast::ArrayLit {
                            span: DUMMY_SP,
                            elems,
                        })),
                    }]);
                }
            } else if ident.sym.as_ref() == "Set"
                && type_args.params.len() == 1
                && let Some(elem_hir) = Self::ts_type_to_hir(&type_args.params[0])
                && let Some(elem_enc) = encode_hir_type(&elem_hir)
            {
                let mut elems = vec![
                    Some(Self::str_arg(SET_TYPE_MARKER)),
                    Some(Self::str_arg(&elem_enc)),
                ];
                if let Some(first) = node
                    .args
                    .take()
                    .and_then(|mut args| (!args.is_empty()).then(|| args.swap_remove(0)))
                {
                    elems.push(Some(first));
                }
                node.args = Some(vec![ast::ExprOrSpread {
                    spread: None,
                    expr: Box::new(ast::Expr::Array(ast::ArrayLit {
                        span: DUMMY_SP,
                        elems,
                    })),
                }]);
            }
        }

        fn visit_mut_ts_interface_decl(&mut self, node: &mut ast::TsInterfaceDecl) {
            node.visit_mut_children_with(self);
            for ext in &mut node.extends {
                let Some(type_args) = ext.type_args.as_ref() else {
                    continue;
                };
                let ast::Expr::Ident(ident) = ext.expr.as_mut() else {
                    continue;
                };
                let mut args = Vec::with_capacity(type_args.params.len());
                let mut ok = true;
                for param in &type_args.params {
                    if let Some(ty) = Self::ts_type_to_hir(param) {
                        args.push(ty);
                    } else {
                        ok = false;
                        break;
                    }
                }
                if ok
                    && let Some(encoded) = encode_hir_type(&HirType::Generic {
                        base: ident.sym.to_string(),
                        type_args: args,
                    })
                {
                    ident.sym = encoded.into();
                    ext.type_args = None;
                }
            }
        }
    }

    module.visit_mut_with(&mut TypeArgPreserver);
}

fn encode_hir_type(ty: &HirType) -> Option<String> {
    Some(match ty {
        HirType::String => "s".to_string(),
        HirType::Number => "n".to_string(),
        HirType::Boolean => "b".to_string(),
        HirType::BigInt => "i".to_string(),
        HirType::Void => "v".to_string(),
        HirType::Null => "z".to_string(),
        HirType::Any => "a".to_string(),
        HirType::Unknown => "u".to_string(),
        HirType::Array(inner) => format!("A:{}", encode_hir_type(inner)?),
        HirType::Named(name) => format!("N:{}:{name}", name.len()),
        HirType::StringLiteral(s) => format!("L:{}:{s}", s.len()),
        HirType::Tuple(items) => {
            let mut out = format!("T:{}:", items.len());
            for item in items {
                let enc = encode_hir_type(item)?;
                out.push_str(&format!("{}:{enc}", enc.len()));
            }
            out
        }
        HirType::Union(items) => {
            let mut out = format!("U:{}:", items.len());
            for item in items {
                let enc = encode_hir_type(item)?;
                out.push_str(&format!("{}:{enc}", enc.len()));
            }
            out
        }
        HirType::Generic { base, type_args } => {
            let mut out = format!("G:{}:{base}:{}:", base.len(), type_args.len());
            for arg in type_args {
                let enc = encode_hir_type(arg)?;
                out.push_str(&format!("{}:{enc}", enc.len()));
            }
            out
        }
        _ => return None,
    })
}

fn take_len_prefixed(rest: &str) -> Option<(&str, &str)> {
    let (len_str, after_colon) = rest.split_once(':')?;
    let len: usize = len_str.parse().ok()?;
    if after_colon.len() < len {
        return None;
    }
    Some((&after_colon[..len], &after_colon[len..]))
}

pub(crate) fn decode_hir_type(s: &str) -> Option<HirType> {
    match s {
        "s" => return Some(HirType::String),
        "n" => return Some(HirType::Number),
        "b" => return Some(HirType::Boolean),
        "i" => return Some(HirType::BigInt),
        "v" => return Some(HirType::Void),
        "z" => return Some(HirType::Null),
        "a" => return Some(HirType::Any),
        "u" => return Some(HirType::Unknown),
        _ => {}
    }
    if let Some(rest) = s.strip_prefix("A:") {
        return Some(HirType::Array(Box::new(decode_hir_type(rest)?)));
    }
    if let Some(rest) = s.strip_prefix("N:") {
        let (name, rem) = take_len_prefixed(rest)?;
        return rem.is_empty().then_some(HirType::Named(name.to_string()));
    }
    if let Some(rest) = s.strip_prefix("L:") {
        let (lit, rem) = take_len_prefixed(rest)?;
        return rem
            .is_empty()
            .then_some(HirType::StringLiteral(lit.to_string()));
    }
    if let Some(rest) = s.strip_prefix("T:") {
        let (count_str, mut rem) = rest.split_once(':')?;
        let count: usize = count_str.parse().ok()?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let (chunk, next) = take_len_prefixed(rem)?;
            items.push(decode_hir_type(chunk)?);
            rem = next;
        }
        return rem.is_empty().then_some(HirType::Tuple(items));
    }
    if let Some(rest) = s.strip_prefix("U:") {
        let (count_str, mut rem) = rest.split_once(':')?;
        let count: usize = count_str.parse().ok()?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let (chunk, next) = take_len_prefixed(rem)?;
            items.push(decode_hir_type(chunk)?);
            rem = next;
        }
        return rem.is_empty().then_some(HirType::Union(items));
    }
    if let Some(rest) = s.strip_prefix("G:") {
        let (base, rem) = take_len_prefixed(rest)?;
        let rem = rem.strip_prefix(':')?;
        let (count_str, mut rem) = rem.split_once(':')?;
        let count: usize = count_str.parse().ok()?;
        let mut args = Vec::with_capacity(count);
        for _ in 0..count {
            let (chunk, next) = take_len_prefixed(rem)?;
            args.push(decode_hir_type(chunk)?);
            rem = next;
        }
        return rem.is_empty().then_some(HirType::Generic {
            base: base.to_string(),
            type_args: args,
        });
    }
    None
}

pub(super) fn parse_map_new_arg(expr: &Expr) -> Option<(HirType, HirType, Option<&Expr>)> {
    let Expr::Array(elems) = expr else {
        return None;
    };
    let Some(Expr::String(marker)) = elems.first() else {
        return None;
    };
    if marker != MAP_TYPE_MARKER {
        return None;
    }
    let Expr::String(k_enc) = elems.get(1)? else {
        return None;
    };
    let Expr::String(v_enc) = elems.get(2)? else {
        return None;
    };
    let init = elems.get(3).filter(|e| !matches!(e, Expr::Undefined));
    Some((decode_hir_type(k_enc)?, decode_hir_type(v_enc)?, init))
}

pub(super) fn parse_set_new_arg(expr: &Expr) -> Option<(HirType, Option<&Expr>)> {
    let Expr::Array(elems) = expr else {
        return None;
    };
    let Some(Expr::String(marker)) = elems.first() else {
        return None;
    };
    if marker != SET_TYPE_MARKER {
        return None;
    }
    let Expr::String(t_enc) = elems.get(1)? else {
        return None;
    };
    let init = elems.get(2).filter(|e| !matches!(e, Expr::Undefined));
    Some((decode_hir_type(t_enc)?, init))
}

pub(super) fn parse_map_type_args(args: &[Expr]) -> Option<(HirType, HirType, Option<&Expr>)> {
    args.first().and_then(parse_map_new_arg)
}

pub(super) fn parse_set_type_args(args: &[Expr]) -> Option<(HirType, Option<&Expr>)> {
    args.first().and_then(parse_set_new_arg)
}

/// Extract `(K, V, tuple_rec)` if `ty` is an `ArrayType` whose element is a 2-field tuple `RecordType`.
pub(super) fn tuple_kv_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let ty = optional_value(ctx, ty).unwrap_or(ty);
    let arr = TypedHandle::<ArrayType>::from_handle(ty, ctx).ok()?;
    let elem = arr.deref(ctx).element();
    let rec = TypedHandle::<RecordType>::from_handle(elem, ctx).ok()?;
    let rec_ref = rec.deref(ctx);
    if !rec_ref.is_tuple() {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    if fields.len() != 2 {
        return None;
    }
    let (_, k_ty) = fields.iter().find(|(n, _)| n == "_0")?;
    let (_, v_ty) = fields.iter().find(|(n, _)| n == "_1")?;
    Some((*k_ty, *v_ty, rec))
}

/// Extract `(K, V, tuple_rec)` if `ty` is a `Map<K, V>` (`map_tuple_*` element).
pub(super) fn map_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let (k, v, rec) = tuple_kv_types(ctx, ty)?;
    rec.deref(ctx)
        .name()
        .as_ref()
        .starts_with("map_tuple_")
        .then_some((k, v, rec))
}

/// Extract `(K, V, tuple_rec)` if `ty` is a `Record<K, V>` / dictionary (`dict_tuple_*` element).
pub(super) fn dict_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let (k, v, rec) = tuple_kv_types(ctx, ty)?;
    rec.deref(ctx)
        .name()
        .as_ref()
        .starts_with("dict_tuple_")
        .then_some((k, v, rec))
}

/// Extract `(K, V, tuple_rec)` if `ty` is either a `Map<K, V>`, `Record<K, V>`, or `[K, V][]`.
pub(super) fn kv_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    tuple_kv_types(ctx, ty)
}

/// Extract `(T, tuple_rec)` if `ty` is a `Set<T>` (`set_tuple_*` element).
pub(super) fn set_entry_type(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypedHandle<RecordType>)> {
    let ty = optional_value(ctx, ty).unwrap_or(ty);
    let arr = TypedHandle::<ArrayType>::from_handle(ty, ctx).ok()?;
    let elem = arr.deref(ctx).element();
    let rec = TypedHandle::<RecordType>::from_handle(elem, ctx).ok()?;
    let rec_ref = rec.deref(ctx);
    if !rec_ref.is_tuple() || !rec_ref.name().as_ref().starts_with("set_tuple_") {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    if fields.len() != 1 {
        return None;
    }
    let (_, elem_ty) = fields.iter().find(|(n, _)| n == "_0")?;
    Some((*elem_ty, rec))
}

/// Check if a closed `RecordType` has fields that can all be unified or read as values.
fn record_uniform_field_type(
    ctx: &Context,
    rec: TypedHandle<RecordType>,
) -> Option<(Vec<(String, TypeHandle)>, TypeHandle)> {
    let rec_ref = rec.deref(ctx);
    if rec_ref.is_tuple() {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    if fields.is_empty() {
        return None;
    }
    let first_ty = optional_value(ctx, fields[0].1).unwrap_or(fields[0].1);
    let all_same = fields
        .iter()
        .all(|(_, ty)| optional_value(ctx, *ty).unwrap_or(*ty) == first_ty);
    if !all_same {
        return None;
    }
    let any_optional = fields
        .iter()
        .any(|(_, ty)| optional_value(ctx, *ty).is_some());
    let unified = if any_optional {
        fields
            .iter()
            .find(|(_, ty)| optional_value(ctx, *ty).is_some())
            .map(|(_, ty)| *ty)
            .unwrap_or(first_ty)
    } else {
        first_ty
    };
    Some((fields.to_vec(), unified))
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn collection_expression(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Option<Value>> {
        match expr {
            Expr::MapNew => Ok(Some(self.empty_map(ctx, expected)?)),
            Expr::MapNewFromArray(init) => {
                if let Some((k_hir, v_hir, real_init)) = parse_map_new_arg(init) {
                    let map_ty = self
                        .shapes
                        .kv_array_type(ctx, &k_hir, &v_hir, "map_tuple")?;
                    if let Some(real_init) = real_init {
                        return Ok(Some(self.map_from_entries(ctx, real_init, Some(map_ty))?));
                    }
                    return Ok(Some(self.empty_map(ctx, Some(map_ty))?));
                }
                Ok(Some(self.map_from_entries(ctx, init, expected)?))
            }
            Expr::SetNew => Ok(Some(self.empty_set(ctx, expected)?)),
            Expr::SetNewFromArray(init) => {
                if let Some((elem_hir, real_init)) = parse_set_new_arg(init) {
                    let set_ty = self.shapes.set_array_type(ctx, &elem_hir)?;
                    if let Some(real_init) = real_init {
                        return Ok(Some(self.set_from_array(ctx, real_init, Some(set_ty))?));
                    }
                    return Ok(Some(self.empty_set(ctx, Some(set_ty))?));
                }
                Ok(Some(self.set_from_array(ctx, init, expected)?))
            }
            Expr::New {
                class_name, args, ..
            } if class_name == "Map" => {
                if let Some((k_hir, v_hir, real_init)) = parse_map_type_args(args) {
                    let map_ty = self
                        .shapes
                        .kv_array_type(ctx, &k_hir, &v_hir, "map_tuple")?;
                    if let Some(real_init) = real_init {
                        return Ok(Some(self.map_from_entries(ctx, real_init, Some(map_ty))?));
                    }
                    return Ok(Some(self.empty_map(ctx, Some(map_ty))?));
                }
                if let [init] = args.as_slice()
                    && !matches!(init, Expr::Undefined)
                {
                    return Ok(Some(self.map_from_entries(ctx, init, expected)?));
                }
                Ok(Some(self.empty_map(ctx, expected)?))
            }
            Expr::New {
                class_name, args, ..
            } if class_name == "Set" => {
                if let Some((elem_hir, real_init)) = parse_set_type_args(args) {
                    let set_ty = self.shapes.set_array_type(ctx, &elem_hir)?;
                    if let Some(real_init) = real_init {
                        return Ok(Some(self.set_from_array(ctx, real_init, Some(set_ty))?));
                    }
                    return Ok(Some(self.empty_set(ctx, Some(set_ty))?));
                }
                if let [init] = args.as_slice()
                    && !matches!(init, Expr::Undefined)
                {
                    return Ok(Some(self.set_from_array(ctx, init, expected)?));
                }
                Ok(Some(self.empty_set(ctx, expected)?))
            }
            Expr::MapSize(map) | Expr::SetSize(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                Ok(Some(self.array_length(ctx, m)))
            }
            Expr::MapGet { map, key } => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                let (k_ty, v_ty, _) = kv_entry_types(ctx, m.get_type(ctx))
                    .context("Map.get receiver must be a Map")?;
                let k = self.expression_with(ctx, key, Some(k_ty))?;
                Ok(Some(self.kv_get(ctx, m, k, k_ty, v_ty)?))
            }
            Expr::MapSet { map, key, value } => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                let (k_ty, v_ty, tuple_rec) = kv_entry_types(ctx, m.get_type(ctx))
                    .context("Map.set receiver must be a Map")?;
                let k = self.expression_with(ctx, key, Some(k_ty))?;
                let v = self.expression_with(ctx, value, Some(v_ty))?;
                Ok(Some(self.kv_set(ctx, m, k, v, k_ty, v_ty, tuple_rec)?))
            }
            Expr::MapHas { map, key } => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                let (k_ty, _, _) = kv_entry_types(ctx, m.get_type(ctx))
                    .context("Map.has receiver must be a Map")?;
                let k = self.expression_with(ctx, key, Some(k_ty))?;
                Ok(Some(self.kv_has(ctx, m, k, k_ty)?))
            }
            Expr::MapDelete { map, key } => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                let (k_ty, _, _) = kv_entry_types(ctx, m.get_type(ctx))
                    .context("Map.delete receiver must be a Map")?;
                let k = self.expression_with(ctx, key, Some(k_ty))?;
                Ok(Some(self.kv_delete(ctx, m, k, k_ty)?))
            }
            Expr::MapClear(map) | Expr::SetClear(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                Ok(Some(self.collection_clear(ctx, m)?))
            }
            Expr::MapValues(map) | Expr::SetValues(map) | Expr::ArrayValues(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                let ty = m.get_type(ctx);
                if let Some((elem_ty, _)) = set_entry_type(ctx, ty) {
                    return Ok(Some(self.set_values(ctx, m, elem_ty)?));
                }
                if kv_entry_types(ctx, ty).is_some() {
                    return Ok(Some(self.kv_values(ctx, m)?));
                }
                Ok(Some(m))
            }
            Expr::MapKeys(map) | Expr::ArrayKeys(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                let ty = m.get_type(ctx);
                if let Some((elem_ty, _)) = set_entry_type(ctx, ty) {
                    return Ok(Some(self.set_values(ctx, m, elem_ty)?));
                }
                if kv_entry_types(ctx, ty).is_some() {
                    return Ok(Some(self.kv_keys(ctx, m)?));
                }
                Ok(Some(self.array_keys(ctx, m)?))
            }
            Expr::MapEntries(map) | Expr::ArrayEntries(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                let ty = m.get_type(ctx);
                if let Some((elem_ty, _)) = set_entry_type(ctx, ty) {
                    return Ok(Some(self.set_entries(ctx, m, elem_ty)?));
                }
                if kv_entry_types(ctx, ty).is_some() {
                    return Ok(Some(m));
                }
                let elem_ty = self.array_element(ctx, m)?;
                Ok(Some(self.array_entries(ctx, m, elem_ty)?))
            }
            Expr::SetAdd { set_id, value } => {
                let slot = self.local(*set_id)?;
                let s = self.load(ctx, slot)?;
                let s = self.defined(ctx, s)?;
                let (elem_ty, tuple_rec) = set_entry_type(ctx, s.get_type(ctx))
                    .context("Set.add receiver must be a Set")?;
                let v = self.expression_with(ctx, value, Some(elem_ty))?;
                Ok(Some(self.set_add(ctx, s, v, elem_ty, tuple_rec)?))
            }
            Expr::SetHas { set, value } => {
                let s = self.expression(ctx, set)?;
                let s = self.defined(ctx, s)?;
                let (elem_ty, _) = set_entry_type(ctx, s.get_type(ctx))
                    .context("Set.has receiver must be a Set")?;
                let v = self.expression_with(ctx, value, Some(elem_ty))?;
                Ok(Some(self.set_has(ctx, s, v, elem_ty)?))
            }
            Expr::SetDelete { set, value } => {
                let s = self.expression(ctx, set)?;
                let s = self.defined(ctx, s)?;
                let (elem_ty, _) = set_entry_type(ctx, s.get_type(ctx))
                    .context("Set.delete receiver must be a Set")?;
                let v = self.expression_with(ctx, value, Some(elem_ty))?;
                Ok(Some(self.set_delete(ctx, s, v, elem_ty)?))
            }
            Expr::ObjectValues(obj) => Ok(Some(self.object_values(ctx, obj)?)),
            Expr::ObjectEntries(obj) => Ok(Some(self.object_entries(ctx, obj)?)),
            Expr::ObjectFromEntries(entries) => {
                Ok(Some(self.object_from_entries(ctx, entries, expected)?))
            }
            Expr::ObjectIs(a, b) => Ok(Some(self.object_is(ctx, a, b)?)),
            Expr::ArrayIsArray(v) => Ok(Some(self.array_is_array(ctx, v)?)),
            Expr::In { property, object } => Ok(Some(self.in_operator(ctx, property, object)?)),
            Expr::Delete(inner) => Ok(Some(self.delete_operator(ctx, inner)?)),
            _ => Ok(None),
        }
    }

    pub(super) fn collection_method(
        &mut self,
        ctx: &mut Context,
        _object: &'s Expr,
        receiver: Value,
        method: &str,
        args: &'s [Expr],
    ) -> Result<Option<Option<Value>>> {
        let recv_ty = receiver.get_type(ctx);
        if let Some((k_ty, v_ty, tuple_rec)) = map_entry_types(ctx, recv_ty) {
            let res = match method {
                "get" => {
                    let [key] = args else {
                        bail!("Map.get requires one key argument");
                    };
                    let k = self.expression_with(ctx, key, Some(k_ty))?;
                    Some(self.kv_get(ctx, receiver, k, k_ty, v_ty)?)
                }
                "set" => {
                    let [key, value] = args else {
                        bail!("Map.set requires key and value arguments");
                    };
                    let k = self.expression_with(ctx, key, Some(k_ty))?;
                    let v = self.expression_with(ctx, value, Some(v_ty))?;
                    Some(self.kv_set(ctx, receiver, k, v, k_ty, v_ty, tuple_rec)?)
                }
                "has" => {
                    let [key] = args else {
                        bail!("Map.has requires one key argument");
                    };
                    let k = self.expression_with(ctx, key, Some(k_ty))?;
                    Some(self.kv_has(ctx, receiver, k, k_ty)?)
                }
                "delete" => {
                    let [key] = args else {
                        bail!("Map.delete requires one key argument");
                    };
                    let k = self.expression_with(ctx, key, Some(k_ty))?;
                    Some(self.kv_delete(ctx, receiver, k, k_ty)?)
                }
                "clear" => {
                    ensure!(args.is_empty(), "Map.clear takes no arguments");
                    Some(self.collection_clear(ctx, receiver)?)
                }
                "keys" => {
                    ensure!(args.is_empty(), "Map.keys takes no arguments");
                    Some(self.kv_keys(ctx, receiver)?)
                }
                "values" => {
                    ensure!(args.is_empty(), "Map.values takes no arguments");
                    Some(self.kv_values(ctx, receiver)?)
                }
                "entries" => {
                    ensure!(args.is_empty(), "Map.entries takes no arguments");
                    Some(receiver)
                }
                "forEach" => {
                    let [callback] = args else {
                        bail!("Map.forEach requires one callback argument");
                    };
                    Some(self.map_for_each(ctx, receiver, callback)?)
                }
                _ => return Ok(None),
            };
            return Ok(Some(res));
        }

        if let Some((elem_ty, tuple_rec)) = set_entry_type(ctx, recv_ty) {
            let res = match method {
                "add" => {
                    let [value] = args else {
                        bail!("Set.add requires one value argument");
                    };
                    let v = self.expression_with(ctx, value, Some(elem_ty))?;
                    Some(self.set_add(ctx, receiver, v, elem_ty, tuple_rec)?)
                }
                "has" => {
                    let [value] = args else {
                        bail!("Set.has requires one value argument");
                    };
                    let v = self.expression_with(ctx, value, Some(elem_ty))?;
                    Some(self.set_has(ctx, receiver, v, elem_ty)?)
                }
                "delete" => {
                    let [value] = args else {
                        bail!("Set.delete requires one value argument");
                    };
                    let v = self.expression_with(ctx, value, Some(elem_ty))?;
                    Some(self.set_delete(ctx, receiver, v, elem_ty)?)
                }
                "clear" => {
                    ensure!(args.is_empty(), "Set.clear takes no arguments");
                    Some(self.collection_clear(ctx, receiver)?)
                }
                "keys" | "values" => {
                    ensure!(args.is_empty(), "Set.{method} takes no arguments");
                    Some(self.set_values(ctx, receiver, elem_ty)?)
                }
                "entries" => {
                    ensure!(args.is_empty(), "Set.entries takes no arguments");
                    Some(self.set_entries(ctx, receiver, elem_ty)?)
                }
                "forEach" => {
                    let [callback] = args else {
                        bail!("Set.forEach requires one callback argument");
                    };
                    Some(self.set_for_each(ctx, receiver, callback)?)
                }
                _ => return Ok(None),
            };
            return Ok(Some(res));
        }

        if kind(ctx, receiver)? == Kind::Array && dict_entry_types(ctx, recv_ty).is_none() {
            let elem_ty = self.array_element(ctx, receiver)?;
            let res = match method {
                "keys" => {
                    ensure!(args.is_empty(), "Array.keys takes no arguments");
                    Some(self.array_keys(ctx, receiver)?)
                }
                "values" => {
                    ensure!(args.is_empty(), "Array.values takes no arguments");
                    Some(receiver)
                }
                "entries" => {
                    ensure!(args.is_empty(), "Array.entries takes no arguments");
                    Some(self.array_entries(ctx, receiver, elem_ty)?)
                }
                "forEach" => {
                    let [callback] = args else {
                        bail!("forEach requires one callback argument");
                    };
                    Some(self.array_for_each(ctx, receiver, callback)?)
                }
                _ => return Ok(None),
            };
            return Ok(Some(res));
        }

        Ok(None)
    }

    fn empty_map(&mut self, ctx: &mut Context, expected: Option<TypeHandle>) -> Result<Value> {
        let Some(expected) = expected else {
            bail!("an empty Map requires a type annotation (e.g. Map<K, V>)");
        };
        let expected = optional_value(ctx, expected).unwrap_or(expected);
        let (_, _, tuple_rec) =
            kv_entry_types(ctx, expected).context("expected a Map<K, V> type")?;
        Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![])))
    }

    fn map_from_entries(
        &mut self,
        ctx: &mut Context,
        init: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let entries_val = self.expression_with(ctx, init, expected)?;
        let entries_val = self.defined(ctx, entries_val)?;
        let (k_ty, v_ty, _) = kv_entry_types(ctx, entries_val.get_type(ctx))
            .or_else(|| expected.and_then(|e| kv_entry_types(ctx, e)))
            .context("Map constructor requires an iterable of [key, value] tuples")?;
        let map_tuple = match expected.and_then(|e| map_entry_types(ctx, e).map(|(_, _, r)| r)) {
            Some(r) => r,
            None => self.shapes.mir_record_prefix(
                ctx,
                vec![("_0".to_string(), k_ty), ("_1".to_string(), v_ty)],
                Some("map_tuple"),
            )?,
        };
        let map = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, map_tuple.into(), vec![]));
        let limit = self.array_length(ctx, entries_val);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "map_init", zero, limit, |b, ctx, idx, _, _| {
            let entry = b.element_at(ctx, entries_val, idx)?;
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let k = b.append_value(ctx, &k_op);
            let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
            let v = b.append_value(ctx, &v_op);
            b.kv_set(ctx, map, k, v, k_ty, v_ty, map_tuple)?;
            Ok(())
        })?;
        Ok(map)
    }

    fn empty_set(&mut self, ctx: &mut Context, expected: Option<TypeHandle>) -> Result<Value> {
        let Some(expected) = expected else {
            bail!("an empty Set requires a type annotation (e.g. Set<T>)");
        };
        let expected = optional_value(ctx, expected).unwrap_or(expected);
        let (_, tuple_rec) =
            set_entry_type(ctx, expected).context("expected a Set<T> type")?;
        Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![])))
    }

    fn set_from_array(
        &mut self,
        ctx: &mut Context,
        init: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let init_expected = expected.and_then(|exp| {
            set_entry_type(ctx, exp).map(|(elem_ty, _)| ArrayType::get(ctx, elem_ty).into())
        });
        let arr = self.expression_with(ctx, init, init_expected)?;
        let arr = self.defined(ctx, arr)?;
        let (elem_ty, tuple_rec, source_is_set) = match set_entry_type(ctx, arr.get_type(ctx)) {
            Some((e, r)) => (e, r, true),
            None => {
                let e = self.array_element(ctx, arr)?;
                let r = match expected.and_then(|exp| set_entry_type(ctx, exp).map(|(_, r)| r)) {
                    Some(r) => r,
                    None => self.shapes.mir_record_prefix(
                        ctx,
                        vec![("_0".to_string(), e)],
                        Some("set_tuple"),
                    )?,
                };
                (e, r, false)
            }
        };
        let set = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "set_init", zero, limit, |b, ctx, idx, _, _| {
            let item = if source_is_set {
                b.set_element_at(ctx, arr, idx)?
            } else {
                b.element_at(ctx, arr, idx)?
            };
            b.set_add(ctx, set, item, elem_ty, tuple_rec)?;
            Ok(())
        })?;
        Ok(set)
    }

    pub(super) fn same_value_zero(
        &mut self,
        ctx: &mut Context,
        a: Value,
        b: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        let eq = self.compare(ctx, CompareKind::Eq, a, b);
        if ty.deref(ctx).is::<FP64Type>() {
            let bool_ty = eq.get_type(ctx);
            let a_nan = self.compare(ctx, CompareKind::Ne, a, a);
            let b_nan = self.compare(ctx, CompareKind::Ne, b, b);
            let both_nan = self.choose(ctx, bool_ty, a_nan, |_, _| Ok(b_nan), |_, _| Ok(a_nan))?;
            self.choose(ctx, bool_ty, eq, |_, _| Ok(eq), |_, _| Ok(both_nan))
        } else {
            Ok(eq)
        }
    }
}
