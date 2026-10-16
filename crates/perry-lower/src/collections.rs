//! Lowering for `Map<K, V>`, `Set<T>`, `Record<K, V>`, `Object.*`, `Array.isArray`, and `in`.
//!
//! A `Map` or `Set` is a box record `{ _0: entries, _1: live count, _2: cursors }`. Deletes
//! tombstone entries, and every live iterator registers a cursor in `_2` so compaction can
//! move it; views (`map.keys()`) own such a cursor. Records are recognized by name prefix.

use super::*;
use perry_mir::types::OptionalType;

mod keyed;
mod objects;
mod storage;
pub(super) use storage::LiveCursor;

/// The entry records of a live `Map` or `Set`, which carry a liveness flag for tombstones.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    Map,
    Set,
}

impl EntryKind {
    /// The entry record's liveness flag.
    fn live_field(self) -> &'static str {
        match self {
            Self::Map => "_2",
            Self::Set => "_1",
        }
    }

    /// The name prefix of the collection's box record.
    pub(super) fn box_prefix(self) -> &'static str {
        match self {
            Self::Map => "map_box",
            Self::Set => "set_box",
        }
    }

    /// Classify an entry record; dictionary and plain tuples have no liveness flag.
    fn of_entry(ctx: &Context, rec: TypedHandle<RecordType>) -> Option<Self> {
        let rec_ref = rec.deref(ctx);
        let name = rec_ref.name();
        if name.as_ref().starts_with("map_tuple_") {
            Some(Self::Map)
        } else if name.as_ref().starts_with("set_tuple_") {
            Some(Self::Set)
        } else {
            None
        }
    }
}

/// `keys()`, `values()` or `entries()`, the iterator methods of `Map`, `Set` and arrays.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IteratorMethod {
    Keys,
    Values,
    Entries,
}

impl IteratorMethod {
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "keys" => Some(Self::Keys),
            "values" => Some(Self::Values),
            "entries" => Some(Self::Entries),
            _ => None,
        }
    }

    /// The live view this method returns on a `Map`.
    pub(super) fn map_view(self) -> CollectionViewKind {
        match self {
            Self::Keys => CollectionViewKind::MapKeys,
            Self::Values => CollectionViewKind::MapValues,
            Self::Entries => CollectionViewKind::MapEntries,
        }
    }

    /// The live view this method returns on a `Set`, whose keys are its values.
    pub(super) fn set_view(self) -> CollectionViewKind {
        match self {
            Self::Keys | Self::Values => CollectionViewKind::SetValues,
            Self::Entries => CollectionViewKind::SetEntries,
        }
    }
}

/// A live iterator view over a `Map` or `Set` box.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CollectionViewKind {
    MapKeys,
    MapValues,
    MapEntries,
    SetValues,
    SetEntries,
}

impl CollectionViewKind {
    const ALL: [Self; 5] = [
        Self::MapKeys,
        Self::MapValues,
        Self::MapEntries,
        Self::SetValues,
        Self::SetEntries,
    ];

    /// The generic name inference gives the view (`MapKeys<K, V>`, `SetValues<T>`).
    pub(super) fn type_name(self) -> &'static str {
        match self {
            Self::MapKeys => "MapKeys",
            Self::MapValues => "MapValues",
            Self::MapEntries => "MapEntries",
            Self::SetValues => "SetValues",
            Self::SetEntries => "SetEntries",
        }
    }

    pub(super) fn from_type_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|view| view.type_name() == name)
    }

    /// The name prefix of the view's record.
    pub(super) fn record_prefix(self) -> &'static str {
        match self {
            Self::MapKeys => "map_keys_box",
            Self::MapValues => "map_values_box",
            Self::MapEntries => "map_entries_box",
            Self::SetValues => "set_values_box",
            Self::SetEntries => "set_entries_box",
        }
    }

    /// The collection the view reads.
    pub(super) fn source(self) -> EntryKind {
        match self {
            Self::MapKeys | Self::MapValues | Self::MapEntries => EntryKind::Map,
            Self::SetValues | Self::SetEntries => EntryKind::Set,
        }
    }
}

/// The record behind `ty` (optional or not) if its name is `{prefix}_…`.
fn prefixed_record(ctx: &Context, ty: TypeHandle, prefix: &str) -> Option<TypedHandle<RecordType>> {
    let ty = optional_value(ctx, ty).unwrap_or(ty);
    let rec = TypedHandle::<RecordType>::from_handle(ty, ctx).ok()?;
    rec.deref(ctx)
        .name()
        .as_ref()
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('_'))
        .then_some(rec)
}

pub(super) fn map_box_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    prefixed_record(ctx, ty, EntryKind::Map.box_prefix())
}

pub(super) fn set_box_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    prefixed_record(ctx, ty, EntryKind::Set.box_prefix())
}

pub(super) fn collection_view_record(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(CollectionViewKind, TypedHandle<RecordType>)> {
    CollectionViewKind::ALL
        .into_iter()
        .find_map(|view| Some((view, prefixed_record(ctx, ty, view.record_prefix())?)))
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
        if let Some(constructor) = self.shapes.metadata.constructors.get(expr) {
            use hir_compat::Constructor;
            return match constructor {
                Constructor::Map { types, init } => {
                    let expected = match types {
                        Some((key, value)) => Some(self.shapes.map_type(ctx, key, value)?),
                        None => expected,
                    };
                    match init {
                        Some(init) => self.map_from_entries(ctx, init, expected),
                        None => self.empty_map(ctx, expected),
                    }
                    .map(Some)
                }
                Constructor::Set { element, init } => {
                    let expected = match element {
                        Some(element) => Some(self.shapes.set_type(ctx, element)?),
                        None => expected,
                    };
                    match init {
                        Some(init) => self.set_from_array(ctx, init, expected),
                        None => self.empty_set(ctx, expected),
                    }
                    .map(Some)
                }
            };
        }
        match expr {
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
        self.alloc_collection_box(ctx, tuple_rec, EntryKind::Map)
    }

    fn map_from_entries(
        &mut self,
        ctx: &mut Context,
        init: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let init_expected =
            if let Some((k_ty, v_ty, _)) = expected.and_then(|exp| kv_entry_types(ctx, exp)) {
                let pair_rec = self.shapes.pair_type(ctx, k_ty, v_ty)?;
                Some(ArrayType::get(ctx, pair_rec.into()).into())
            } else {
                None
            };
        let entries_val = self.expression_with(ctx, init, init_expected)?;
        let entries_val = self.defined(ctx, entries_val)?;
        let (k_ty, v_ty) = iterable_kv_types(ctx, entries_val.get_type(ctx))
            .or_else(|| expected.and_then(|e| iterable_kv_types(ctx, e)))
            .context("Map constructor requires an iterable of [key, value] tuples")?;
        let map_tuple = match expected.and_then(|e| map_entry_types(ctx, e).map(|(_, _, r)| r)) {
            Some(r) => r,
            None => self.shapes.map_tuple_type(ctx, k_ty, v_ty)?,
        };
        let map = self.alloc_collection_box(ctx, map_tuple, EntryKind::Map)?;
        self.live_collection_loop(ctx, "map_init", entries_val, |b, ctx, _, entry| {
            let (k, v) = b.entry_fields(ctx, entry)?;
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
        self.alloc_collection_box(ctx, tuple_rec, EntryKind::Set)
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
        let (elem_ty, tuple_rec) = match set_entry_type(ctx, arr.get_type(ctx)) {
            Some((e, r)) => (e, r),
            None => {
                let e = self.iterable_element_type(ctx, arr.get_type(ctx))?;
                let r = match expected.and_then(|exp| set_entry_type(ctx, exp).map(|(_, r)| r)) {
                    Some(r) => r,
                    None => self.shapes.set_tuple_type(ctx, e)?,
                };
                (e, r)
            }
        };
        let set = self.alloc_collection_box(ctx, tuple_rec, EntryKind::Set)?;
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
