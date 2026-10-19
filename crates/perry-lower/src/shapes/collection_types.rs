//! MIR records of `Map`, `Set`, their live views, dictionaries and pairs.
use super::*;
use crate::collections::{CollectionViewKind, EntryKind, StorageField as Field};

impl Shapes<'_> {
    /// The `tuple` record `[first, second]`: iterated entries and `[key, value]` pairs.
    pub(crate) fn pair_type(
        &self,
        ctx: &Context,
        first: TypeHandle,
        second: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_tuple(
            ctx,
            vec![("_0".to_string(), first), ("_1".to_string(), second)],
        )
    }

    /// A `Map` entry `{ _0: key, _1: value, _2: live, _3: seq }`.
    pub(crate) fn map_tuple_type(
        &self,
        ctx: &Context,
        k_ty: TypeHandle,
        v_ty: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
        let i1_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let f64_ty = FP64Type::get(ctx).into();
        self.mir_record_kind(
            ctx,
            RecordKind::MapEntry,
            vec![
                ("_0".to_string(), k_ty),
                ("_1".to_string(), v_ty),
                ("_2".to_string(), i1_ty),
                ("_3".to_string(), f64_ty),
            ],
        )
    }

    /// A `Set` entry `{ _0: value, _1: live, _2: seq }`.
    pub(crate) fn set_tuple_type(
        &self,
        ctx: &Context,
        elem_ty: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
        let i1_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let f64_ty = FP64Type::get(ctx).into();
        self.mir_record_kind(
            ctx,
            RecordKind::SetEntry,
            vec![
                ("_0".to_string(), elem_ty),
                ("_1".to_string(), i1_ty),
                ("_2".to_string(), f64_ty),
            ],
        )
    }

    /// The `{ _0: index, _1: min_seq, _2: epoch }` cursor of a live iterator.
    pub(crate) fn cursor_tuple_type(&self, ctx: &Context) -> Result<TypedHandle<RecordType>> {
        let f64_ty: TypeHandle = FP64Type::get(ctx).into();
        self.mir_tuple(
            ctx,
            vec![
                (Field::CursorIndex.name().to_owned(), f64_ty),
                (Field::CursorSequence.name().to_owned(), f64_ty),
                (Field::CursorEpoch.name().to_owned(), f64_ty),
            ],
        )
    }

    /// A `kind` box over `entry` records: `{ _0: entries, _1: live count, _2: next_seq, _3: epoch }`.
    pub(crate) fn collection_box_type(
        &self,
        ctx: &Context,
        entry: TypedHandle<RecordType>,
        kind: EntryKind,
    ) -> Result<TypedHandle<RecordType>> {
        let entries_arr = ArrayType::get(ctx, entry.into()).into();
        let f64_ty: TypeHandle = FP64Type::get(ctx).into();
        self.mir_record_kind(
            ctx,
            kind.box_kind(),
            vec![
                (Field::BoxEntries.name().to_owned(), entries_arr),
                (Field::BoxLength.name().to_owned(), f64_ty),
                (Field::BoxSequence.name().to_owned(), f64_ty),
                (Field::BoxEpoch.name().to_owned(), f64_ty),
            ],
        )
    }

    /// A live `view` `{ _0: box, _1: cursor }` over a box of type `box_ty`.
    pub(crate) fn collection_view_type(
        &self,
        ctx: &Context,
        box_ty: TypeHandle,
        view: CollectionViewKind,
    ) -> Result<TypedHandle<RecordType>> {
        let cursor_rec = self.cursor_tuple_type(ctx)?;
        self.mir_record_kind(
            ctx,
            view.record_kind(),
            vec![
                (Field::ViewBox.name().to_owned(), box_ty),
                (Field::ViewCursor.name().to_owned(), cursor_rec.into()),
            ],
        )
    }

    /// `Map<K, V>` from source types.
    pub(crate) fn map_type(&self, ctx: &Context, k: &HirType, v: &HirType) -> Result<TypeHandle> {
        let k_ty = self.value_type(ctx, k)?;
        let v_ty = self.value_type(ctx, v)?;
        Ok(self.map_box_type(ctx, k_ty, v_ty)?.into())
    }

    /// `Set<T>` from a source type.
    pub(crate) fn set_type(&self, ctx: &Context, elem: &HirType) -> Result<TypeHandle> {
        let elem_ty = self.value_type(ctx, elem)?;
        Ok(self.set_box_type(ctx, elem_ty)?.into())
    }

    fn map_box_type(
        &self,
        ctx: &Context,
        k_ty: TypeHandle,
        v_ty: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
        let entry = self.map_tuple_type(ctx, k_ty, v_ty)?;
        self.collection_box_type(ctx, entry, EntryKind::Map)
    }

    fn set_box_type(&self, ctx: &Context, elem_ty: TypeHandle) -> Result<TypedHandle<RecordType>> {
        let entry = self.set_tuple_type(ctx, elem_ty)?;
        self.collection_box_type(ctx, entry, EntryKind::Set)
    }

    /// A `Record<K, V>` dictionary entry `{ _0: K, _1: V }`.
    pub(crate) fn dict_tuple_type(
        &self,
        ctx: &Context,
        k_ty: TypeHandle,
        v_ty: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_record_kind(
            ctx,
            RecordKind::DictionaryEntry,
            vec![("_0".to_string(), k_ty), ("_1".to_string(), v_ty)],
        )
    }

    /// A `Record<K, V>` dictionary from source types.
    pub(super) fn dict_source_type(
        &self,
        ctx: &Context,
        k: &HirType,
        v: &HirType,
    ) -> Result<TypeHandle> {
        let k_ty = self.value_type(ctx, k)?;
        let v_ty = self.value_type(ctx, v)?;
        let entry = self.dict_tuple_type(ctx, k_ty, v_ty)?;
        Ok(ArrayType::get(ctx, entry.into()).into())
    }

    /// A live `view` from its generic arguments: `[K, V]` over a `Map`, `[T]` over a `Set`.
    pub(super) fn collection_view_source_type(
        &self,
        ctx: &Context,
        view: CollectionViewKind,
        arguments: &[HirType],
    ) -> Result<TypeHandle> {
        let box_rec = match (view.source(), arguments) {
            (EntryKind::Map, [k, v]) => self.map_type(ctx, k, v)?,
            (EntryKind::Set, [element]) => self.set_type(ctx, element)?,
            (EntryKind::Map, _) => bail!("{} needs key and value type arguments", view.type_name()),
            (EntryKind::Set, _) => bail!("{} needs one element type argument", view.type_name()),
        };
        Ok(self.collection_view_type(ctx, box_rec, view)?.into())
    }
}
