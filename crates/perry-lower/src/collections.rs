//! Lowering for `Map<K, V>`, `Set<T>`, `Record<K, V>`, `Object.*`, `Array.isArray`, and `in`.

use super::*;
use perry_mir::types::OptionalType;
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{VisitMut, VisitMutWith};

mod objects;

/// Rewrite `new Map<K, V>(...)` and `new Set<T>(...)` in the AST before HIR lowering
/// so explicit constructor type arguments are preserved even when the variable binding
/// has no type annotation.
pub(super) fn ts_type_to_hir(ty: &ast::TsType) -> Option<HirType> {
    match ty {
        ast::TsType::TsKeywordType(kw) => match kw.kind {
            ast::TsKeywordTypeKind::TsStringKeyword => Some(HirType::String),
            ast::TsKeywordTypeKind::TsNumberKeyword => Some(HirType::Number),
            ast::TsKeywordTypeKind::TsBooleanKeyword => Some(HirType::Boolean),
            ast::TsKeywordTypeKind::TsBigIntKeyword => Some(HirType::BigInt),
            ast::TsKeywordTypeKind::TsVoidKeyword | ast::TsKeywordTypeKind::TsUndefinedKeyword => {
                Some(HirType::Void)
            }
            ast::TsKeywordTypeKind::TsNullKeyword => Some(HirType::Null),
            ast::TsKeywordTypeKind::TsAnyKeyword => Some(HirType::Any),
            ast::TsKeywordTypeKind::TsUnknownKeyword => Some(HirType::Unknown),
            _ => None,
        },
        ast::TsType::TsArrayType(arr) => {
            Some(HirType::Array(Box::new(ts_type_to_hir(&arr.elem_type)?)))
        }
        ast::TsType::TsTupleType(tuple) => {
            let mut elems = Vec::with_capacity(tuple.elem_types.len());
            for elem in &tuple.elem_types {
                elems.push(ts_type_to_hir(&elem.ty)?);
            }
            Some(HirType::Tuple(elems))
        }
        ast::TsType::TsUnionOrIntersectionType(ast::TsUnionOrIntersectionType::TsUnionType(u)) => {
            let mut items = Vec::with_capacity(u.types.len());
            for member in &u.types {
                items.push(ts_type_to_hir(member)?);
            }
            Some(HirType::Union(items))
        }
        ast::TsType::TsParenthesizedType(p) => ts_type_to_hir(&p.type_ann),
        ast::TsType::TsTypeRef(type_ref) => {
            let ast::TsEntityName::Ident(ident) = &type_ref.type_name else {
                return None;
            };
            let name = ident.sym.as_ref();
            if let Some(params) = &type_ref.type_params {
                let mut args = Vec::with_capacity(params.params.len());
                for param in &params.params {
                    args.push(ts_type_to_hir(param)?);
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

/// Propagate `new Map<K, V>(...)` and `new Set<T>(...)` type arguments onto unannotated
/// variable bindings while preserving the builtin constructor identifier for `perry_hir`.
pub(super) fn preserve_type_args(module: &mut ast::Module) {
    struct TypeArgPreserver;

    impl VisitMut for TypeArgPreserver {
        fn visit_mut_var_declarator(&mut self, node: &mut ast::VarDeclarator) {
            node.visit_mut_children_with(self);
            let ast::Pat::Ident(binding) = &mut node.name else {
                return;
            };
            if binding.type_ann.is_some() {
                return;
            }
            let Some(init) = node.init.as_ref() else {
                return;
            };
            let ast::Expr::New(new_expr) = init.as_ref() else {
                return;
            };
            let ast::Expr::Ident(ident) = new_expr.callee.as_ref() else {
                return;
            };
            let Some(type_args) = new_expr.type_args.as_ref() else {
                return;
            };
            let sym = ident.sym.as_ref();
            if (sym == "Map" && type_args.params.len() == 2)
                || (sym == "Set" && type_args.params.len() == 1)
            {
                binding.type_ann = Some(Box::new(ast::TsTypeAnn {
                    span: swc_common::DUMMY_SP,
                    type_ann: Box::new(ast::TsType::TsTypeRef(ast::TsTypeRef {
                        span: swc_common::DUMMY_SP,
                        type_name: ast::TsEntityName::Ident(ident.clone()),
                        type_params: Some(type_args.clone()),
                    })),
                }));
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
                    if let Some(ty) = ts_type_to_hir(param) {
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

pub(super) fn map_box_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    let ty = optional_value(ctx, ty).unwrap_or(ty);
    let rec = TypedHandle::<RecordType>::from_handle(ty, ctx).ok()?;
    rec.deref(ctx)
        .name()
        .as_ref()
        .starts_with("map_box_")
        .then_some(rec)
}

pub(super) fn set_box_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    let ty = optional_value(ctx, ty).unwrap_or(ty);
    let rec = TypedHandle::<RecordType>::from_handle(ty, ctx).ok()?;
    rec.deref(ctx)
        .name()
        .as_ref()
        .starts_with("set_box_")
        .then_some(rec)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CollectionViewKind {
    MapKeys,
    MapValues,
    MapEntries,
    SetValues,
    SetEntries,
}

pub(super) fn collection_view_record(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(CollectionViewKind, TypedHandle<RecordType>)> {
    let ty = optional_value(ctx, ty).unwrap_or(ty);
    let rec = TypedHandle::<RecordType>::from_handle(ty, ctx).ok()?;
    let rec_ref = rec.deref(ctx);
    let name_ref = rec_ref.name().as_ref();
    if name_ref.starts_with("map_keys_box_") {
        Some((CollectionViewKind::MapKeys, rec))
    } else if name_ref.starts_with("map_values_box_") {
        Some((CollectionViewKind::MapValues, rec))
    } else if name_ref.starts_with("map_entries_box_") {
        Some((CollectionViewKind::MapEntries, rec))
    } else if name_ref.starts_with("set_values_box_") {
        Some((CollectionViewKind::SetValues, rec))
    } else if name_ref.starts_with("set_entries_box_") {
        Some((CollectionViewKind::SetEntries, rec))
    } else {
        None
    }
}

pub(super) fn is_collection_or_view_record(ctx: &Context, ty: TypeHandle) -> bool {
    map_box_record(ctx, ty).is_some()
        || set_box_record(ctx, ty).is_some()
        || collection_view_record(ctx, ty).is_some()
}

/// Extract `(K, V, tuple_rec)` if `ty` is an `ArrayType` whose element is a key-value tuple `RecordType`.
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
    let is_map_tuple = rec_ref.name().as_ref().starts_with("map_tuple_");
    if !(fields.len() == 2 || (is_map_tuple && fields.len() == 3)) {
        return None;
    }
    let (_, k_ty) = fields.iter().find(|(n, _)| n == "_0")?;
    let (_, v_ty) = fields.iter().find(|(n, _)| n == "_1")?;
    Some((*k_ty, *v_ty, rec))
}

/// Extract `(K, V, tuple_rec)` if `ty` is a `Map<K, V>` (`map_box_*` or `map_tuple_*` element).
pub(super) fn map_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    if let Some(box_rec) = map_box_record(ctx, ty) {
        let (_, entries_ty) = box_rec.deref(ctx).field("_0")?;
        return tuple_kv_types(ctx, entries_ty);
    }
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
    if let Some(res) = map_entry_types(ctx, ty) {
        return Some(res);
    }
    tuple_kv_types(ctx, ty)
}

/// Extract `(K, V)` from any iterable of `[K, V]` pairs (`Map`, `Record`, `[K, V][]`, `MapEntries`, `SetEntries`).
pub(super) fn iterable_kv_types(ctx: &Context, ty: TypeHandle) -> Option<(TypeHandle, TypeHandle)> {
    if let Some((k, v, _)) = kv_entry_types(ctx, ty) {
        return Some((k, v));
    }
    if let Some((view_kind, view_rec)) = collection_view_record(ctx, ty) {
        let (_, inner_box) = view_rec.deref(ctx).field("_0")?;
        return match view_kind {
            CollectionViewKind::MapEntries => {
                let (k, v, _) = map_entry_types(ctx, inner_box)?;
                Some((k, v))
            }
            CollectionViewKind::SetEntries => {
                let (elem, _) = set_entry_type(ctx, inner_box)?;
                Some((elem, elem))
            }
            _ => None,
        };
    }
    None
}

/// Extract `(T, tuple_rec)` if `ty` is a `Set<T>` (`set_box_*` or `set_tuple_*` element).
pub(super) fn set_entry_type(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypedHandle<RecordType>)> {
    let ty = optional_value(ctx, ty).unwrap_or(ty);
    let entries_ty = if let Some(box_rec) = set_box_record(ctx, ty) {
        box_rec.deref(ctx).field("_0")?.1
    } else {
        ty
    };
    let arr = TypedHandle::<ArrayType>::from_handle(entries_ty, ctx).ok()?;
    let elem = arr.deref(ctx).element();
    let rec = TypedHandle::<RecordType>::from_handle(elem, ctx).ok()?;
    let rec_ref = rec.deref(ctx);
    if !rec_ref.is_tuple() || !rec_ref.name().as_ref().starts_with("set_tuple_") {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    if fields.len() != 2 {
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
            Expr::MapNew => {
                let idx = self.map_ctor_idx;
                self.map_ctor_idx += 1;
                let expected = if let Some((k_hir, v_hir)) =
                    self.shapes.locations.map_ctor(&self.fn_name, idx)
                {
                    Some(
                        self.shapes
                            .kv_array_type(ctx, &k_hir, &v_hir, "map_tuple")?,
                    )
                } else {
                    expected
                };
                Ok(Some(self.empty_map(ctx, expected)?))
            }
            Expr::MapNewFromArray(init) => {
                let idx = self.map_ctor_idx;
                self.map_ctor_idx += 1;
                let expected = if let Some((k_hir, v_hir)) =
                    self.shapes.locations.map_ctor(&self.fn_name, idx)
                {
                    Some(
                        self.shapes
                            .kv_array_type(ctx, &k_hir, &v_hir, "map_tuple")?,
                    )
                } else {
                    expected
                };
                Ok(Some(self.map_from_entries(ctx, init, expected)?))
            }
            Expr::SetNew => {
                let idx = self.set_ctor_idx;
                self.set_ctor_idx += 1;
                let expected =
                    if let Some(elem_hir) = self.shapes.locations.set_ctor(&self.fn_name, idx) {
                        Some(self.shapes.set_array_type(ctx, &elem_hir)?)
                    } else {
                        expected
                    };
                Ok(Some(self.empty_set(ctx, expected)?))
            }
            Expr::SetNewFromArray(init) => {
                let idx = self.set_ctor_idx;
                self.set_ctor_idx += 1;
                let expected =
                    if let Some(elem_hir) = self.shapes.locations.set_ctor(&self.fn_name, idx) {
                        Some(self.shapes.set_array_type(ctx, &elem_hir)?)
                    } else {
                        expected
                    };
                Ok(Some(self.set_from_array(ctx, init, expected)?))
            }
            Expr::New {
                class_name, args, ..
            } if class_name == "Map" => {
                let idx = self.map_ctor_idx;
                self.map_ctor_idx += 1;
                let expected = if let Some((k_hir, v_hir)) =
                    self.shapes.locations.map_ctor(&self.fn_name, idx)
                {
                    Some(
                        self.shapes
                            .kv_array_type(ctx, &k_hir, &v_hir, "map_tuple")?,
                    )
                } else {
                    expected
                };
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
                let idx = self.set_ctor_idx;
                self.set_ctor_idx += 1;
                let expected =
                    if let Some(elem_hir) = self.shapes.locations.set_ctor(&self.fn_name, idx) {
                        Some(self.shapes.set_array_type(ctx, &elem_hir)?)
                    } else {
                        expected
                    };
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
                Ok(Some(self.collection_size(ctx, m)?))
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
                    return Ok(Some(self.kv_entries(ctx, m)?));
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
                    Some(self.kv_entries(ctx, receiver)?)
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
        self.alloc_map_box(ctx, tuple_rec)
    }

    fn map_from_entries(
        &mut self,
        ctx: &mut Context,
        init: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let init_expected =
            if let Some((k_ty, v_ty, _)) = expected.and_then(|exp| kv_entry_types(ctx, exp)) {
                let pair_rec = self.shapes.mir_record_prefix(
                    ctx,
                    vec![("_0".to_string(), k_ty), ("_1".to_string(), v_ty)],
                    Some("tuple"),
                )?;
                Some(ArrayType::get(ctx, pair_rec.into()).into())
            } else {
                None
            };
        let entries_val = self.expression_with(ctx, init, init_expected)?;
        let entries_val = self.defined(ctx, entries_val)?;
        let (k_ty, v_ty) = iterable_kv_types(ctx, entries_val.get_type(ctx))
            .or_else(|| expected.and_then(|e| iterable_kv_types(ctx, e)))
            .context("Map constructor requires an iterable of [key, value] tuples")?;
        let i1_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let map_tuple = match expected.and_then(|e| map_entry_types(ctx, e).map(|(_, _, r)| r)) {
            Some(r) => r,
            None => self.shapes.mir_record_prefix(
                ctx,
                vec![
                    ("_0".to_string(), k_ty),
                    ("_1".to_string(), v_ty),
                    ("_2".to_string(), i1_ty),
                ],
                Some("map_tuple"),
            )?,
        };
        let map = self.alloc_map_box(ctx, map_tuple)?;
        self.live_collection_loop(ctx, "map_init", entries_val, |b, ctx, _, entry| {
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
        let (_, tuple_rec) = set_entry_type(ctx, expected).context("expected a Set<T> type")?;
        self.alloc_set_box(ctx, tuple_rec)
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
        let i1_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let (elem_ty, tuple_rec) = match set_entry_type(ctx, arr.get_type(ctx)) {
            Some((e, r)) => (e, r),
            None => {
                let e = self.iterable_element_type(ctx, arr.get_type(ctx))?;
                let r = match expected.and_then(|exp| set_entry_type(ctx, exp).map(|(_, r)| r)) {
                    Some(r) => r,
                    None => self.shapes.mir_record_prefix(
                        ctx,
                        vec![("_0".to_string(), e), ("_1".to_string(), i1_ty)],
                        Some("set_tuple"),
                    )?,
                };
                (e, r)
            }
        };
        let set = self.alloc_set_box(ctx, tuple_rec)?;
        self.live_collection_loop(ctx, "set_init", arr, |b, ctx, _, item| {
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

    /// Convert a JSON object snapshot `{"k": v, ...}` into a `Record<string, V>` dictionary array.
    pub(crate) fn decode_dict(
        &mut self,
        ctx: &mut Context,
        value: Value,
        _k_ty: TypeHandle,
        v_ty: TypeHandle,
        tuple_rec: TypedHandle<RecordType>,
    ) -> Result<Value> {
        self.intrinsic_void(ctx, Intrinsic::JsonRequireObject, vec![value]);
        let keys = self.intrinsic(ctx, Intrinsic::JsonKeys, vec![value]);
        let dict = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        let limit = self.array_length(ctx, keys);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "json_dict", zero, limit, |b, ctx, idx, _, _| {
            let key_str = b.element_at(ctx, keys, idx)?;
            let raw_val = b.intrinsic(ctx, Intrinsic::JsonMember, vec![value, key_str]);
            let decoded_val = b.fit(ctx, raw_val, v_ty)?;
            let entry = b.emit_value(ctx, |ctx| {
                AllocOp::new(ctx, tuple_rec, vec![key_str, decoded_val])
            });
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, dict, entry));
            Ok(())
        })?;
        Ok(dict)
    }
}
