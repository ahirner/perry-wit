//! MIR records of `Map`, `Set`, their live views, dictionaries and pairs.
use super::*;
use crate::lower::collections::{
    CollectionKind, CollectionViewKind, EntryTypes, StorageField as Field,
};

impl<T> Shapes<'_, T> {
    /// The `tuple` record `[first, second]`: iterated entries and `[key, value]` pairs.
    pub(crate) fn pair_type(
        &mut self,
        ctx: &Context,
        first: TypeHandle,
        second: TypeHandle,
    ) -> Result<TypedHandle<RecordType>> {
        self.mir_tuple(
            ctx,
            vec![("_0".to_string(), first), ("_1".to_string(), second)],
        )
    }

    pub(in crate::lower) fn entry_type(
        &mut self,
        ctx: &Context,
        types: EntryTypes,
    ) -> Result<TypedHandle<RecordType>> {
        let (kind, record, mut fields) = match types {
            EntryTypes::Map(key, value) => (
                CollectionKind::Map,
                RecordKind::MapEntry,
                vec![("_0".into(), key), ("_1".into(), value)],
            ),
            EntryTypes::Set(element) => (
                CollectionKind::Set,
                RecordKind::SetEntry,
                vec![("_0".into(), element)],
            ),
        };
        fields.push((
            kind.live_field().into(),
            IntegerType::get(ctx, 1, Signedness::Signless).into(),
        ));
        fields.push((kind.seq_field().into(), FP64Type::get(ctx).into()));
        self.mir_record_kind(ctx, record, fields)
    }

    /// The `{ _0: index, _1: min_seq, _2: epoch }` cursor of a live iterator.
    pub(crate) fn cursor_tuple_type(&mut self, ctx: &Context) -> Result<TypedHandle<RecordType>> {
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
        &mut self,
        ctx: &Context,
        entry: TypedHandle<RecordType>,
        kind: CollectionKind,
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
        &mut self,
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
}

impl<T> Shapes<'_, T> {
    pub(in crate::lower) fn collection_type(
        &mut self,
        ctx: &Context,
        types: EntryTypes,
    ) -> Result<TypedHandle<RecordType>> {
        let entry = self.entry_type(ctx, types)?;
        let kind = match types {
            EntryTypes::Map(..) => CollectionKind::Map,
            EntryTypes::Set(_) => CollectionKind::Set,
        };
        self.collection_box_type(ctx, entry, kind)
    }

    /// A `Record<K, V>` dictionary entry `{ _0: K, _1: V }`.
    pub(crate) fn dict_tuple_type(
        &mut self,
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
}

impl Shapes<'_, Preparing<'_>> {
    pub(super) fn builtin_generic(
        &mut self,
        ctx: &Context,
        name: &str,
        arguments: &[Ty],
        visiting: &mut Vec<crate::engine::types::LocId>,
    ) -> Result<Option<TypeHandle>> {
        let mut carrier =
            |this: &mut Self, ty: &Ty| this.project_type(ctx, *ty, visiting).map(|b| b.carrier);
        if name != "WitInput" && self.is_user_type(name) {
            return Ok(None);
        }
        let view = CollectionViewKind::from_type_name(name);
        let collection = match name {
            "Map" | "ReadonlyMap" => Some(CollectionKind::Map),
            "Set" | "ReadonlySet" => Some(CollectionKind::Set),
            _ => view.map(CollectionViewKind::source),
        };
        if let Some(collection) = collection {
            let storage = match (collection, arguments) {
                (CollectionKind::Map, [key, value]) => {
                    let key = carrier(self, key)?;
                    let value = carrier(self, value)?;
                    self.collection_type(ctx, EntryTypes::Map(key, value))?
                }
                (CollectionKind::Set, [element]) => {
                    let element = carrier(self, element)?;
                    self.collection_type(ctx, EntryTypes::Set(element))?
                }
                (CollectionKind::Map, _) => bail_diag!(
                    codes::TYPE_MISMATCH,
                    "{name} needs key and value type arguments"
                ),
                (CollectionKind::Set, _) => bail_diag!(
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
            ("WitInput", [arg]) => {
                let arg = carrier(self, arg)?;
                self.input_type(ctx, arg)?
            }
            ("PromiseSettledResult", [arg]) => {
                perry_mir::SettledType::get(ctx, carrier(self, arg)?).into()
            }
            ("ReadonlyArray" | "Array", [arg]) => ArrayType::get(ctx, carrier(self, arg)?).into(),
            ("Readonly", [arg]) => carrier(self, arg)?,
            ("Record", [key, value]) => {
                let key = carrier(self, key)?;
                let value = carrier(self, value)?;
                let entry = self.dict_tuple_type(ctx, key, value)?;
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
