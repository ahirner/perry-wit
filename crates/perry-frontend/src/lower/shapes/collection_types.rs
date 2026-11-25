//! MIR records of `Map`, `Set`, their live views, dictionaries and pairs.
use super::*;
use crate::lower::collections::{CollectionViewKind, EntryKind, StorageField as Field};

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

    pub(super) fn map_box_type(
        &self,
        ctx: &Context,
        k_ty: TypeHandle,
        v_ty: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
        let entry = self.map_tuple_type(ctx, k_ty, v_ty)?;
        self.collection_box_type(ctx, entry, EntryKind::Map)
    }

    pub(super) fn set_box_type(
        &self,
        ctx: &Context,
        elem_ty: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
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

    pub(super) fn builtin_generic<T>(
        &self,
        ctx: &Context,
        name: &str,
        arguments: &[T],
        mut carrier: impl FnMut(&T) -> Result<TypeHandle>,
    ) -> Result<Option<TypeHandle>> {
        if name != "WitInput" && self.is_user_type(name) {
            return Ok(None);
        }
        let view = CollectionViewKind::from_type_name(name);
        let collection = match name {
            "Map" | "ReadonlyMap" => Some(EntryKind::Map),
            "Set" | "ReadonlySet" => Some(EntryKind::Set),
            _ => view.map(CollectionViewKind::source),
        };
        if let Some(collection) = collection {
            let storage = match (collection, arguments) {
                (EntryKind::Map, [key, value]) => {
                    self.map_box_type(ctx, carrier(key)?, carrier(value)?)?
                }
                (EntryKind::Set, [element]) => self.set_box_type(ctx, carrier(element)?)?,
                (EntryKind::Map, _) => bail_diag!(
                    codes::TYPE_MISMATCH,
                    "{name} needs key and value type arguments"
                ),
                (EntryKind::Set, _) => bail_diag!(
                    codes::TYPE_MISMATCH,
                    "{name} needs one element type argument"
                ),
            };
            return Ok(Some(match view {
                Some(view) => self.collection_view_type(ctx, storage.into(), view)?.into(),
                None => storage.into(),
            }));
        }
        Ok(Some(match (name, arguments) {
            ("WitInput", [arg]) => self.input_type(ctx, carrier(arg)?)?,
            ("PromiseSettledResult", [arg]) => {
                perry_mir::SettledType::get(ctx, carrier(arg)?).into()
            }
            ("ReadonlyArray" | "Array", [arg]) => ArrayType::get(ctx, carrier(arg)?).into(),
            ("Readonly", [arg]) => carrier(arg)?,
            ("Record", [key, value]) => {
                let entry = self.dict_tuple_type(ctx, carrier(key)?, carrier(value)?)?;
                ArrayType::get(ctx, entry.into()).into()
            }
            ("Record", _) => bail_diag!(
                codes::TYPE_MISMATCH,
                "{name} needs key and value type arguments"
            ),
            ("WitInput" | "PromiseSettledResult", _) => {
                bail_diag!(codes::TYPE_MISMATCH, "{name} needs one type argument")
            }
            ("ReadonlyArray" | "Array", _) => bail_diag!(
                codes::TYPE_MISMATCH,
                "{name} needs one element type argument"
            ),
            _ => return Ok(None),
        }))
    }
}
