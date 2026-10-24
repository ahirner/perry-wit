//! Lowering for `Map<K, V>`, `Set<T>`, `Record<K, V>`, `Object.*`, `Array.isArray`, and `in`.
//!
//! A `Map` or `Set` is a box record `{ _0: entries, _1: live count, _2: next_seq, _3: epoch }`.
//! Deletes tombstone entries, and live iterators track `(index, min_seq, epoch)` so clears and
//! compactions resynchronize cursor positions against monotonic entry sequence numbers.

use super::*;
use crate::control_flow::{ArrayOutput, Holes, Input, Projection};
use crate::hir_compat::Constructor;
use perry_mir::{OptionalType, RecordKind};

mod keyed;
pub(super) use keyed::{SetOperation, SetRelation};
mod objects;
mod storage;
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

    /// Classify a `Map` or `Set` entry record by its `[live, key, value?]` field layout.
    fn of_entry(ctx: &Context, rec: TypedHandle<RecordType>) -> Option<Self> {
        match rec.deref(ctx).kind() {
            RecordKind::MapEntry => Some(Self::Map),
            RecordKind::SetEntry => Some(Self::Set),
            _ => None,
        }
    }
}

pub(super) use perry_frontend::collections::{CollectionViewKind, IteratorMethod};
pub(super) trait CollectionViewExt {
    fn record_kind(self) -> RecordKind;
    fn source(self) -> EntryKind;
}
impl CollectionViewExt for CollectionViewKind {
    /// The view's record role.
    fn record_kind(self) -> RecordKind {
        match self {
            Self::MapKeys => RecordKind::MapKeys,
            Self::MapValues => RecordKind::MapValues,
            Self::MapEntries => RecordKind::MapEntries,
            Self::SetValues => RecordKind::SetValues,
            Self::SetEntries => RecordKind::SetEntries,
        }
    }

    /// The collection the view reads.
    fn source(self) -> EntryKind {
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

/// The key, value and entry types of a `Map<K, V>` box.
pub(super) fn map_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let box_rec = map_box_record(ctx, ty)?;
    let (_, entries_ty) = box_rec.deref(ctx).field(StorageField::BoxEntries.name())?;
    tuple_kv_types(ctx, entries_ty)
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
    Projection::collection(ctx, ty).ok()?.pair_types(ctx)
}

/// The element and entry types of a `Set<T>` box.
pub(super) fn set_entry_type(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypedHandle<RecordType>)> {
    let box_rec = set_box_record(ctx, ty)?;
    let entries_ty = box_rec.deref(ctx).field(StorageField::BoxEntries.name())?.1;
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
        match expr {
            Expr::MapSize(map) | Expr::SetSize(map) => {
                let m = self.expression(ctx, map)?;
                let m = self.defined(ctx, m)?;
                Ok(Some(self.collection_size(ctx, m)?))
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
            Expr::ObjectIs(a, b) => {
                if let Some(value) = self.absence_test(ctx, CompareKind::Eq, false, a, b)? {
                    return Ok(Some(value));
                }
                let lhs = self.expression(ctx, a)?;
                let rhs = self.expression(ctx, b)?;
                Ok(Some(self.same_value(
                    ctx,
                    lhs,
                    rhs,
                    crate::operators::Equality::SameValue,
                )?))
            }
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

    pub(super) fn collection_iterator(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: IteratorMethod,
    ) -> Result<Value> {
        let ty = receiver.get_type(ctx);
        if set_box_record(ctx, ty).is_some() {
            return self.alloc_collection_view(ctx, receiver, method.set_view());
        }
        if map_box_record(ctx, ty).is_some() {
            return self.alloc_collection_view(ctx, receiver, method.map_view());
        }
        let keyed = tuple_kv_types(ctx, ty);
        if keyed.is_none() && method == IteratorMethod::Values {
            return Ok(receiver);
        }
        let input = if keyed.is_some() {
            Input::Collection(receiver)
        } else if method == IteratorMethod::Keys {
            Input::UndefinedCount(self.array_length(ctx, receiver))
        } else {
            Input::Array(receiver, Holes::Dense)
        };
        self.collect_items(ctx, input, |b, ctx, item, index, _| {
            let (key, value) = if keyed.is_some() {
                b.entry_fields(ctx, item)?
            } else {
                (index, item)
            };
            Ok(ArrayOutput::Item(match method {
                IteratorMethod::Keys => key,
                IteratorMethod::Values => value,
                IteratorMethod::Entries => b.pair(ctx, key, value)?,
            }))
        })
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
        self.populate_from_entries(ctx, entries_val, map)
    }

    pub(super) fn populate_from_entries(
        &mut self,
        ctx: &mut Context,
        entries: Value,
        target: Value,
    ) -> Result<Value> {
        self.iterate(ctx, Input::Collection(entries), |b, ctx, entry, _, _, _| {
            let (k, v) = b.entry_fields(ctx, entry)?;
            b.kv_set(ctx, target, k, v)?;
            Ok(())
        })?;
        Ok(target)
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
                let e = Projection::collection(ctx, arr.get_type(ctx))?
                    .output_type(ctx, self.shapes)?;
                let r = match expected.and_then(|exp| set_entry_type(ctx, exp).map(|(_, r)| r)) {
                    Some(r) => r,
                    None => self.shapes.set_tuple_type(ctx, e)?,
                };
                (e, r)
            }
        };
        let set = self.alloc_collection_box(ctx, tuple_rec, EntryKind::Set)?;
        self.iterate(ctx, Input::Collection(arr), |b, ctx, item, _, _, _| {
            b.set_add(ctx, set, item, elem_ty)?;
            Ok(())
        })?;
        Ok(set)
    }
}
