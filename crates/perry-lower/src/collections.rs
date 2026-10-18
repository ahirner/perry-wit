//! Lowering for `Map<K, V>`, `Set<T>`, `Record<K, V>`, `Object.*`, `Array.isArray`, and `in`.
//!
//! A `Map` or `Set` is a box record `{ _0: entries, _1: live count, _2: next_seq, _3: epoch }`.
//! Deletes tombstone entries, and live iterators track `(index, min_seq, epoch)` so clears and
//! compactions resynchronize cursor positions against monotonic entry sequence numbers without
//! retaining cursor lists or compaction histories.

use super::*;
use crate::hir_compat::Constructor;
use perry_mir::{OptionalType, RecordKind};

mod keyed;
mod methods;
mod objects;
mod storage;
use methods::{MapCall, SetCall};
pub(super) use storage::LiveCursor;

/// Stable tuple slots shared by collection construction and live iteration.
pub(super) enum StorageField {
    BoxEntries,
    BoxLength,
    BoxSequence,
    BoxEpoch,
    ViewBox,
    ViewCursor,
    CursorIndex,
    CursorSequence,
    CursorEpoch,
}

impl StorageField {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::BoxEntries | Self::ViewBox | Self::CursorIndex => "_0",
            Self::BoxLength | Self::ViewCursor | Self::CursorSequence => "_1",
            Self::BoxSequence | Self::CursorEpoch => "_2",
            Self::BoxEpoch => "_3",
        }
    }
}

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

    /// The entry record's monotonic insertion sequence number.
    fn seq_field(self) -> &'static str {
        match self {
            Self::Map => "_3",
            Self::Set => "_2",
        }
    }

    /// The collection's box record role.
    pub(super) fn box_kind(self) -> RecordKind {
        match self {
            Self::Map => RecordKind::Map,
            Self::Set => RecordKind::Set,
        }
    }

    /// Classify an entry record; dictionary and plain tuples have no liveness flag.
    fn of_entry(ctx: &Context, rec: TypedHandle<RecordType>) -> Option<Self> {
        match rec.deref(ctx).kind() {
            RecordKind::MapEntry => Some(Self::Map),
            RecordKind::SetEntry => Some(Self::Set),
            _ => None,
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
    fn name(self) -> &'static str {
        match self {
            Self::Keys => "keys",
            Self::Values => "values",
            Self::Entries => "entries",
        }
    }

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

    /// The view's record role.
    pub(super) fn record_kind(self) -> RecordKind {
        match self {
            Self::MapKeys => RecordKind::MapKeys,
            Self::MapValues => RecordKind::MapValues,
            Self::MapEntries => RecordKind::MapEntries,
            Self::SetValues => RecordKind::SetValues,
            Self::SetEntries => RecordKind::SetEntries,
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

/// The record behind `ty` (optional or not) with the requested role.
fn record_of_kind(
    ctx: &Context,
    ty: TypeHandle,
    kind: RecordKind,
) -> Option<TypedHandle<RecordType>> {
    let ty = present_type(ctx, ty);
    let rec = TypedHandle::<RecordType>::from_handle(ty, ctx).ok()?;
    (rec.deref(ctx).kind() == kind).then_some(rec)
}

pub(super) fn map_box_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    record_of_kind(ctx, ty, RecordKind::Map)
}

pub(super) fn set_box_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    record_of_kind(ctx, ty, RecordKind::Set)
}

pub(super) fn collection_view_record(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(CollectionViewKind, TypedHandle<RecordType>)> {
    CollectionViewKind::ALL
        .into_iter()
        .find_map(|view| Some((view, record_of_kind(ctx, ty, view.record_kind())?)))
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
    let ty = present_type(ctx, ty);
    let arr = TypedHandle::<ArrayType>::from_handle(ty, ctx).ok()?;
    let elem = arr.deref(ctx).element();
    let rec = TypedHandle::<RecordType>::from_handle(elem, ctx).ok()?;
    let rec_ref = rec.deref(ctx);
    if !rec_ref.is_tuple() {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    let is_map_tuple = rec_ref.kind() == RecordKind::MapEntry;
    if !(fields.len() == 2 || (is_map_tuple && fields.len() == 4)) {
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
        let (_, entries_ty) = box_rec.deref(ctx).field(StorageField::BoxEntries.name())?;
        return tuple_kv_types(ctx, entries_ty);
    }
    let (k, v, rec) = tuple_kv_types(ctx, ty)?;
    (rec.deref(ctx).kind() == RecordKind::MapEntry).then_some((k, v, rec))
}

/// Extract `(K, V, tuple_rec)` if `ty` is a `Record<K, V>` / dictionary (`dict_tuple_*` element).
pub(super) fn dict_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let (k, v, rec) = tuple_kv_types(ctx, ty)?;
    (rec.deref(ctx).kind() == RecordKind::DictionaryEntry).then_some((k, v, rec))
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
        let (_, inner_box) = view_rec.deref(ctx).field(StorageField::ViewBox.name())?;
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
    let ty = present_type(ctx, ty);
    if let Ok(arr) = TypedHandle::<ArrayType>::from_handle(ty, ctx)
        && let Ok(elem_arr) = TypedHandle::<ArrayType>::from_handle(arr.deref(ctx).element(), ctx)
    {
        let inner = elem_arr.deref(ctx).element();
        return Some((inner, inner));
    }
    None
}

/// Extract `(T, tuple_rec)` if `ty` is a `Set<T>` (`set_box_*` or `set_tuple_*` element).
pub(super) fn set_entry_type(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypedHandle<RecordType>)> {
    let ty = present_type(ctx, ty);
    let entries_ty = if let Some(box_rec) = set_box_record(ctx, ty) {
        box_rec.deref(ctx).field(StorageField::BoxEntries.name())?.1
    } else {
        ty
    };
    let arr = TypedHandle::<ArrayType>::from_handle(entries_ty, ctx).ok()?;
    let elem = arr.deref(ctx).element();
    let rec = TypedHandle::<RecordType>::from_handle(elem, ctx).ok()?;
    let rec_ref = rec.deref(ctx);
    if rec_ref.kind() != RecordKind::SetEntry {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    if fields.len() != 3 {
        return None;
    }
    let (_, elem_ty) = fields.iter().find(|(n, _)| n == "_0")?;
    Some((*elem_ty, rec))
}

/// Check if a closed `RecordType` has fields that can all be unified or read as values.
fn record_uniform_field_type(ctx: &Context, rec: TypedHandle<RecordType>) -> Option<TypeHandle> {
    let rec_ref = rec.deref(ctx);
    if rec_ref.is_tuple() {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    if fields.is_empty() {
        return None;
    }
    let first_ty = present_type(ctx, fields[0].1);
    let all_same = fields
        .iter()
        .all(|(_, ty)| present_type(ctx, *ty) == first_ty);
    if !all_same {
        return None;
    }
    Some(
        fields
            .iter()
            .find_map(|(_, ty)| optional_value(ctx, *ty).map(|_| *ty))
            .unwrap_or(first_ty),
    )
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn collection_expression(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Option<Value>> {
        if let Some(constructor) = self.shapes.metadata.constructors.get(expr) {
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
        let map_call = match expr {
            Expr::MapGet { map, key } => Some((map, MapCall::Get(key))),
            Expr::MapSet { map, key, value } => Some((map, MapCall::Set(key, value))),
            Expr::MapHas { map, key } => Some((map, MapCall::Has(key))),
            Expr::MapDelete { map, key } => Some((map, MapCall::Delete(key))),
            _ => None,
        };
        if let Some((map, call)) = map_call {
            let receiver = self.expression(ctx, map)?;
            let receiver = self.defined(ctx, receiver)?;
            return call.lower(self, ctx, receiver).map(Some);
        }
        match expr {
            Expr::MapSize(map) | Expr::SetSize(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                Ok(Some(self.collection_size(ctx, m)?))
            }
            Expr::MapClear(map) | Expr::SetClear(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                Ok(Some(self.collection_clear(ctx, m)?))
            }
            Expr::MapValues(source)
            | Expr::SetValues(source)
            | Expr::ArrayValues(source)
            | Expr::MapKeys(source)
            | Expr::ArrayKeys(source)
            | Expr::MapEntries(source)
            | Expr::ArrayEntries(source) => {
                let receiver = self.expression(ctx, source)?;
                let receiver = self.defined(ctx, receiver)?;
                let method = match expr {
                    Expr::MapKeys(_) | Expr::ArrayKeys(_) => IteratorMethod::Keys,
                    Expr::MapEntries(_) | Expr::ArrayEntries(_) => IteratorMethod::Entries,
                    _ => IteratorMethod::Values,
                };
                Ok(Some(self.collection_iterator(ctx, receiver, method)?))
            }
            Expr::SetAdd { set_id, value } => {
                let slot = self.local(*set_id)?;
                let receiver = self.load(ctx, slot)?;
                let receiver = self.defined(ctx, receiver)?;
                SetCall::Add(value).lower(self, ctx, receiver).map(Some)
            }
            Expr::SetHas { set, value } | Expr::SetDelete { set, value } => {
                let receiver = self.expression(ctx, set)?;
                let receiver = self.defined(ctx, receiver)?;
                let call = if matches!(expr, Expr::SetHas { .. }) {
                    SetCall::Has(value)
                } else {
                    SetCall::Delete(value)
                };
                call.lower(self, ctx, receiver).map(Some)
            }
            Expr::ObjectKeys(obj) if matches!(**obj, Expr::ProcessEnv) => {
                Ok(Some(self.intrinsic(ctx, Intrinsic::EnvKeys, vec![])))
            }
            Expr::ObjectKeys(obj) | Expr::ForInKeys(obj) => {
                Ok(Some(self.object_members(ctx, obj, IteratorMethod::Keys)?))
            }
            Expr::ObjectValues(obj) => Ok(Some(self.object_members(
                ctx,
                obj,
                IteratorMethod::Values,
            )?)),
            Expr::ObjectEntries(obj) => Ok(Some(self.object_members(
                ctx,
                obj,
                IteratorMethod::Entries,
            )?)),
            Expr::ObjectFromEntries(entries) => {
                Ok(Some(self.object_from_entries(ctx, entries, expected)?))
            }
            Expr::ObjectIs(a, b) => Ok(Some(self.object_is(ctx, a, b)?)),
            Expr::ArrayIsArray(v) => Ok(Some(self.array_is_array(ctx, v)?)),
            Expr::In { property, object } => Ok(Some(self.in_operator(ctx, property, object)?)),
            Expr::Delete(inner) => Ok(Some(self.delete_operator(ctx, inner)?)),
            Expr::MapGroupBy { items, key_fn } => {
                Ok(Some(self.map_group_by(ctx, items, key_fn, expected)?))
            }
            Expr::Call { callee, args, .. }
                if matches!(
                    callee.as_ref(),
                    Expr::PropertyGet { object, property, .. }
                        if property == "groupBy"
                            && matches!(
                                object.as_ref(),
                                Expr::PropertyGet { object: g, property: m, .. }
                                    if matches!(g.as_ref(), Expr::GlobalGet(_)) && m == "Map"
                            )
                ) =>
            {
                let [items, key_fn] = args.as_slice() else {
                    bail!("Map.groupBy requires (items, callbackFn)");
                };
                Ok(Some(self.map_group_by(ctx, items, key_fn, expected)?))
            }
            _ => Ok(None),
        }
    }

    pub(super) fn collection_method(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: &str,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let recv_ty = receiver.get_type(ctx);
        if let Some(iterator) = IteratorMethod::parse(method) {
            let collection = if map_entry_types(ctx, recv_ty).is_some() {
                Some("Map")
            } else if set_entry_type(ctx, recv_ty).is_some() {
                Some("Set")
            } else if kind(ctx, receiver)? == Kind::Array
                && dict_entry_types(ctx, recv_ty).is_none()
            {
                Some("Array")
            } else {
                None
            };
            if let Some(collection) = collection {
                ensure!(args.is_empty(), "{collection}.{method} takes no arguments");
                return self.collection_iterator(ctx, receiver, iterator).map(Some);
            }
        }

        if map_entry_types(ctx, recv_ty).is_some() {
            return MapCall::parse(method, args)?
                .map(|call| call.lower(self, ctx, receiver))
                .transpose();
        }
        if set_entry_type(ctx, recv_ty).is_some() {
            return SetCall::parse(method, args)?
                .map(|call| call.lower(self, ctx, receiver))
                .transpose();
        }

        Ok(None)
    }

    fn collection_iterator(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: IteratorMethod,
    ) -> Result<Value> {
        let ty = receiver.get_type(ctx);
        if let Some((element, _)) = set_entry_type(ctx, ty) {
            return match method {
                IteratorMethod::Keys | IteratorMethod::Values => {
                    self.set_values(ctx, receiver, element)
                }
                IteratorMethod::Entries => self.set_entries(ctx, receiver, element),
            };
        }
        if kv_entry_types(ctx, ty).is_some() {
            return match method {
                IteratorMethod::Keys => self.kv_keys(ctx, receiver),
                IteratorMethod::Values => self.kv_values(ctx, receiver),
                IteratorMethod::Entries => self.kv_entries(ctx, receiver),
            };
        }
        match method {
            IteratorMethod::Keys => self.array_keys(ctx, receiver),
            IteratorMethod::Values => Ok(receiver),
            IteratorMethod::Entries => {
                let element = self.array_element(ctx, receiver)?;
                self.array_entries(ctx, receiver, element)
            }
        }
    }

    fn empty_map(&mut self, ctx: &mut Context, expected: Option<TypeHandle>) -> Result<Value> {
        let Some(expected) = expected else {
            bail!("an empty Map requires a type annotation (e.g. Map<K, V>)");
        };
        let expected = present_type(ctx, expected);
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
            b.kv_set(ctx, map, k, v)?;
            Ok(())
        })?;
        Ok(map)
    }

    fn empty_set(&mut self, ctx: &mut Context, expected: Option<TypeHandle>) -> Result<Value> {
        let Some(expected) = expected else {
            bail!("an empty Set requires a type annotation (e.g. Set<T>)");
        };
        let expected = present_type(ctx, expected);
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
