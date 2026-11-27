//! Lowering for `Map<K, V>`, `Set<T>`, `Record<K, V>`, `Object.*`, `Array.isArray`, and `in`.
//!
//! A `Map` or `Set` is a box record `{ _0: entries, _1: live count, _2: next_seq, _3: epoch }`.
//! Deletes tombstone entries, and live iterators track `(index, min_seq, epoch)` so clears and
//! compactions resynchronize cursor positions against monotonic entry sequence numbers.

use super::*;
use crate::lower::control_flow::{ArrayOutput, Holes, Input, Projection};
pub(super) use crate::source::CollectionKind;
use crate::source::ConstructorSites;
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

#[derive(Clone, Copy)]
pub(super) enum EntryTypes {
    Map(TypeHandle, TypeHandle),
    Set(TypeHandle),
}

impl CollectionKind {
    /// The entry record's liveness flag.
    pub(super) fn live_field(self) -> &'static str {
        match self {
            Self::Map => "_2",
            Self::Set => "_1",
        }
    }

    /// The entry record's monotonic insertion sequence number.
    pub(super) fn seq_field(self) -> &'static str {
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

pub(super) use crate::collections::{CollectionViewKind, IteratorMethod};
impl CollectionViewKind {
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
    pub(super) fn source(self) -> CollectionKind {
        match self {
            Self::MapKeys | Self::MapValues | Self::MapEntries => CollectionKind::Map,
            Self::SetValues | Self::SetEntries => CollectionKind::Set,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Collection {
    Box(CollectionKind),
    View(CollectionViewKind),
}

impl Collection {
    pub(super) fn of(ctx: &Context, ty: TypeHandle) -> Option<Self> {
        let record = perry_mir::typed::<RecordType>(ctx, present_type(ctx, ty))?;
        let kind = record.deref(ctx).kind();
        match kind {
            RecordKind::Map => Some(Self::Box(CollectionKind::Map)),
            RecordKind::Set => Some(Self::Box(CollectionKind::Set)),
            _ => CollectionViewKind::ALL
                .into_iter()
                .find(|view| view.record_kind() == kind)
                .map(Self::View),
        }
    }
}

fn array_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    let array = perry_mir::typed::<ArrayType>(ctx, present_type(ctx, ty))?;
    perry_mir::typed(ctx, array.deref(ctx).element())
}

impl CollectionKind {
    pub(super) fn entry(self, ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
        let record = perry_mir::typed::<RecordType>(ctx, present_type(ctx, ty))?;
        let record = record.deref(ctx);
        if record.kind() != self.box_kind() {
            return None;
        }
        array_record(ctx, record.field(StorageField::BoxEntries.name())?.1)
    }
}

pub(super) fn tuple_pair(
    ctx: &Context,
    rec: TypedHandle<RecordType>,
) -> Option<(TypeHandle, TypeHandle)> {
    let record = rec.deref(ctx);
    if !record.is_tuple()
        || !matches!(
            (record.fields().ok()?.len(), record.kind()),
            (2, _) | (4, RecordKind::MapEntry)
        )
    {
        return None;
    }
    Some((record.field("_0")?.1, record.field("_1")?.1))
}

/// Extract `(K, V, tuple_rec)` if `ty` is an `ArrayType` whose element is a key-value tuple `RecordType`.
pub(super) fn tuple_kv_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let rec = array_record(ctx, ty)?;
    let (key, value) = tuple_pair(ctx, rec)?;
    Some((key, value, rec))
}

/// The key, value and entry types of a `Map<K, V>` box.
pub(super) fn map_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let rec = CollectionKind::Map.entry(ctx, ty)?;
    let (key, value) = tuple_pair(ctx, rec)?;
    Some((key, value, rec))
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
    let rec = CollectionKind::Set.entry(ctx, ty)?;
    let record = rec.deref(ctx);
    (record.kind() == RecordKind::SetEntry && record.fields().ok()?.len() == 3)
        .then_some((record.field("_0")?.1, rec))
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

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn new_collection(
        &mut self,
        ctx: &mut Context,
        kind: CollectionKind,
        init: Option<&'s Expr>,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        match (kind, init) {
            (CollectionKind::Map, Some(entries)) => self.map_from_entries(ctx, entries, expected),
            (kind, None) => self.empty_collection(ctx, kind, expected),
            (CollectionKind::Set, Some(elements)) => self.set_from_array(ctx, elements, expected),
        }
    }

    pub(super) fn collection_expression(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Option<Value>> {
        if let Some(constructor) = self.shapes.specialization.instances.collection(expr) {
            return self
                .new_collection(ctx, constructor.kind, constructor.init, expected)
                .map(Some);
        }
        match expr {
            Expr::MapNew => self
                .empty_collection(ctx, CollectionKind::Map, expected)
                .map(Some),
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
                    crate::lower::operators::Equality::SameValue,
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
                    bail_diag!(
                        codes::TYPE_MISMATCH,
                        "Map.groupBy requires (items, callbackFn)"
                    );
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
        if let Some(Collection::Box(kind)) = Collection::of(ctx, ty) {
            let view = match kind {
                CollectionKind::Map => method.map_view(),
                CollectionKind::Set => method.set_view(),
            };
            return self.alloc_collection_view(ctx, receiver, view);
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

    pub(super) fn empty_collection(
        &mut self,
        ctx: &mut Context,
        kind: CollectionKind,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let example = match kind {
            CollectionKind::Map => "Map<K, V>",
            CollectionKind::Set => "Set<T>",
        };
        let expected = expected.ok_or_else(|| {
            diag!(
                codes::MISSING_TYPE_ANNOTATION,
                "an empty {} requires a type annotation (e.g. {example})",
                kind.name()
            )
        })?;
        let entry = match kind {
            CollectionKind::Map => kv_entry_types(ctx, expected).map(|(_, _, entry)| entry),
            CollectionKind::Set => set_entry_type(ctx, expected).map(|(_, entry)| entry),
        }
        .ok_or_else(|| diag!(codes::TYPE_MISMATCH, "expected a {example} type"))?;
        self.alloc_collection_box(ctx, entry, kind)
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
            .ok_or_else(|| {
                Diagnostic::new(
                    codes::TYPE_MISMATCH,
                    "Map constructor requires an iterable of [key, value] tuples",
                )
            })?;
        let map_tuple = match expected.and_then(|e| map_entry_types(ctx, e).map(|(_, _, r)| r)) {
            Some(r) => r,
            None => self.shapes.entry_type(ctx, EntryTypes::Map(k_ty, v_ty))?,
        };
        let map = self.alloc_collection_box(ctx, map_tuple, CollectionKind::Map)?;
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
                    None => self.shapes.entry_type(ctx, EntryTypes::Set(e))?,
                };
                (e, r)
            }
        };
        let set = self.alloc_collection_box(ctx, tuple_rec, CollectionKind::Set)?;
        self.iterate(ctx, Input::Collection(arr), |b, ctx, item, _, _, _| {
            b.set_add(ctx, set, item, elem_ty)?;
            Ok(())
        })?;
        Ok(set)
    }
}
