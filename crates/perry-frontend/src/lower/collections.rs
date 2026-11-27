//! Lowering for `Map<K, V>`, `Set<T>`, `Record<K, V>`, `Object.*`, `Array.isArray`, and `in`.
//!
//! A `Map` or `Set` is a box record `{ _0: entries, _1: live count, _2: next_seq, _3: epoch }`.
//! Deletes tombstone entries, and live iterators track `(index, min_seq, epoch)` so clears and
//! compactions resynchronize cursor positions against monotonic entry sequence numbers.

use super::*;
use crate::collections::Yield;
use crate::lower::control_flow::{ArrayOutput, Holes, Input};
pub(super) use crate::source::CollectionKind;
use crate::source::ConstructorSites;
use perry_mir::{OptionalType, RecordKind};

mod keyed;
pub(super) use keyed::{SetOperation, SetRelation};
mod objects;
mod storage;
pub(super) use storage::LiveCursor;

/// Stable tuple slots shared by collection construction and live iteration.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StorageField {
    Key,
    MapValue,
    Live(CollectionKind),
    Sequence(CollectionKind),
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
            Self::Key | Self::BoxEntries | Self::ViewBox | Self::CursorIndex => "_0",
            Self::MapValue
            | Self::Live(CollectionKind::Set)
            | Self::BoxLength
            | Self::ViewCursor
            | Self::CursorSequence => "_1",
            Self::Sequence(CollectionKind::Set)
            | Self::Live(CollectionKind::Map)
            | Self::BoxSequence
            | Self::CursorEpoch => "_2",
            Self::Sequence(CollectionKind::Map) | Self::BoxEpoch => "_3",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum EntryTypes {
    Map(TypeHandle, TypeHandle),
    Set(TypeHandle),
}

impl CollectionKind {
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
    pub(super) fn fields(self) -> Yield<StorageField> {
        self.project([StorageField::Key, StorageField::MapValue])
    }
}

#[derive(Clone, Copy)]
pub(super) struct CollectionLayout {
    pub(super) view: CollectionViewKind,
    pub(super) boxed: TypedHandle<RecordType>,
    pub(super) entry: TypedHandle<RecordType>,
    pub(super) fields: [TypeHandle; 2],
}

impl<T> Shapes<'_, T> {
    pub(super) fn collection(&self, ctx: &Context, ty: TypeHandle) -> Option<CollectionLayout> {
        self.collections.get(&present_type(ctx, ty)).copied()
    }

    pub(super) fn collection_box(&self, ctx: &Context, ty: TypeHandle) -> Option<CollectionLayout> {
        self.collection(ctx, ty)
            .filter(|layout| TypeHandle::from(layout.boxed) == present_type(ctx, ty))
    }

    pub(super) fn boxed_collection(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        kind: CollectionKind,
    ) -> Option<CollectionLayout> {
        self.collection_box(ctx, ty)
            .filter(|layout| layout.view.source() == kind)
    }
}

fn array_record(ctx: &Context, ty: TypeHandle) -> Option<TypedHandle<RecordType>> {
    let array = perry_mir::typed::<ArrayType>(ctx, present_type(ctx, ty))?;
    perry_mir::typed(ctx, array.deref(ctx).element())
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

/// Extract `(K, V, tuple_rec)` if `ty` is a `Record<K, V>` / dictionary (`dict_tuple_*` element).
pub(super) fn dict_entry_types(
    ctx: &Context,
    ty: TypeHandle,
) -> Option<(TypeHandle, TypeHandle, TypedHandle<RecordType>)> {
    let (k, v, rec) = tuple_kv_types(ctx, ty)?;
    (rec.deref(ctx).kind() == RecordKind::DictionaryEntry).then_some((k, v, rec))
}

impl Shapes<'_> {
    pub(super) fn key_value_types(
        &self,
        ctx: &Context,
        ty: TypeHandle,
    ) -> Option<(TypeHandle, TypeHandle)> {
        if let Some(map) = self.boxed_collection(ctx, ty, CollectionKind::Map) {
            let [key, value] = map.fields;
            return Some((key, value));
        }
        tuple_kv_types(ctx, ty).map(|(key, value, _)| (key, value))
    }

    fn iterable_yield(&self, ctx: &Context, ty: TypeHandle) -> Option<Yield<TypeHandle>> {
        let ty = present_type(ctx, ty);
        if let Some(array) = perry_mir::typed::<ArrayType>(ctx, ty) {
            return Some(Yield::Item(array.deref(ctx).element()));
        }
        let layout = self.collection(ctx, ty)?;
        Some(layout.view.project(layout.fields))
    }

    fn iterable_kv_types(&self, ctx: &Context, ty: TypeHandle) -> Option<(TypeHandle, TypeHandle)> {
        let item = match self.iterable_yield(ctx, ty)? {
            Yield::Pair(key, value) => return Some((key, value)),
            Yield::Item(item) => item,
        };
        if let Some(array) = perry_mir::typed::<ArrayType>(ctx, item) {
            let element = array.deref(ctx).element();
            return Some((element, element));
        }
        tuple_pair(ctx, perry_mir::typed(ctx, item)?)
    }

    fn yield_type(&mut self, ctx: &Context, item: Yield<TypeHandle>) -> Result<TypeHandle> {
        item.materialize(Ok, |fields| {
            self.positional_record(ctx, RecordKind::Tuple, fields)
                .map(Into::into)
        })
    }

    fn inferred_storage(
        &mut self,
        ctx: &Context,
        kind: CollectionKind,
        input: TypeHandle,
        pair: Option<(TypeHandle, TypeHandle)>,
    ) -> Result<CollectionLayout> {
        let entries = match kind {
            CollectionKind::Map => {
                let (key, value) =
                    self.iterable_kv_types(ctx, input).or(pair).ok_or_else(|| {
                        diag!(
                            codes::TYPE_MISMATCH,
                            "Map constructor requires an iterable of [key, value] tuples"
                        )
                    })?;
                EntryTypes::Map(key, value)
            }
            CollectionKind::Set => {
                let item = self
                    .iterable_yield(ctx, input)
                    .ok_or_else(|| unsupported!("expected an array or collection iterable"))?;
                EntryTypes::Set(self.yield_type(ctx, item)?)
            }
        };
        self.collection_type(ctx, entries)
    }
}

/// Check if a closed `RecordType` has fields that can all be unified or read as values.
fn record_uniform_field_type(ctx: &Context, rec: TypedHandle<RecordType>) -> Option<TypeHandle> {
    let rec_ref = rec.deref(ctx);
    if rec_ref.is_tuple() {
        return None;
    }
    let fields = rec_ref.fields().ok()?;
    let first_ty = present_type(ctx, fields.first()?.1);
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
        let declared = expected.and_then(|ty| self.shapes.boxed_collection(ctx, ty, kind));
        let pair = if kind == CollectionKind::Map && declared.is_none() {
            expected
                .and_then(|ty| tuple_kv_types(ctx, ty))
                .map(|(k, v, _)| (k, v))
        } else {
            None
        };
        let Some(init) = init else {
            let layout = match (declared, pair) {
                (Some(layout), _) => layout,
                (_, Some((k, v))) => self.shapes.collection_type(ctx, EntryTypes::Map(k, v))?,
                _ => {
                    let example = match kind {
                        CollectionKind::Map => "Map<K, V>",
                        CollectionKind::Set => "Set<T>",
                    };
                    if expected.is_some() {
                        bail_diag!(codes::TYPE_MISMATCH, "expected a {example} type");
                    }
                    bail_diag!(
                        codes::MISSING_TYPE_ANNOTATION,
                        "an empty {} requires a type annotation (e.g. {example})",
                        kind.name()
                    );
                }
            };
            return Ok(self.alloc_collection_box(ctx, layout));
        };
        let element = declared
            .map(|layout| layout.view.project(layout.fields))
            .or_else(|| pair.map(|(k, v)| Yield::Pair(k, v)))
            .map(|item| self.shapes.yield_type(ctx, item))
            .transpose()?;
        let expected_input = element.map(|ty| ArrayType::get(ctx, ty).into());
        let input = self.expression_with(ctx, init, expected_input)?;
        let input = self.defined(ctx, input)?;
        let layout = match declared {
            Some(layout) => layout,
            None => self
                .shapes
                .inferred_storage(ctx, kind, input.get_type(ctx), pair)?,
        };
        let out = self.alloc_collection_box(ctx, layout);
        match kind {
            CollectionKind::Map => self.populate_from_entries(ctx, input, out),
            CollectionKind::Set => {
                self.iterate(ctx, Input::Collection(input), |b, ctx, item, _, _, _| {
                    b.set_add(ctx, out, item, layout.fields[0])
                })?;
                Ok(out)
            }
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
                .new_collection(ctx, CollectionKind::Map, None, expected)
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
            _ => crate::hir_compat::map_group_by(expr)?
                .map(|(items, key_fn)| self.map_group_by(ctx, items, key_fn, expected))
                .transpose(),
        }
    }

    pub(super) fn collection_iterator(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: IteratorMethod,
    ) -> Result<Value> {
        let ty = receiver.get_type(ctx);
        if let Some(layout) = self.shapes.collection_box(ctx, ty) {
            let kind = layout.view.source();
            return self.alloc_collection_view(ctx, receiver, layout, method.view(kind));
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
            method
                .project([key, value])
                .materialize(Ok, |[k, v]| b.pair(ctx, k, v))
                .map(ArrayOutput::Item)
        })
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
}
