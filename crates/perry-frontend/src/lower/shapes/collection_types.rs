//! MIR records of `Map`, `Set`, their live views, dictionaries and pairs.
use super::*;
use crate::lower::collections::{CollectionKind, CollectionViewKind, EntryTypes};

impl<T> Shapes<'_, T> {
    pub(in crate::lower) fn entry_type(
        &mut self,
        ctx: &Context,
        types: EntryTypes,
    ) -> Result<TypedHandle<RecordType>> {
        let live = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let sequence = FP64Type::get(ctx).into();
        match types {
            EntryTypes::Map(key, value) => {
                self.positional_record(ctx, RecordKind::MapEntry, [key, value, live, sequence])
            }
            EntryTypes::Set(element) => {
                self.positional_record(ctx, RecordKind::SetEntry, [element, live, sequence])
            }
        }
    }

    /// The `{ _0: index, _1: min_seq, _2: epoch }` cursor of a live iterator.
    pub(crate) fn cursor_tuple_type(&mut self, ctx: &Context) -> Result<TypedHandle<RecordType>> {
        let f64_ty: TypeHandle = FP64Type::get(ctx).into();
        self.positional_record(ctx, RecordKind::Tuple, [f64_ty; 3])
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
        self.positional_record(ctx, kind.box_kind(), [entries_arr, f64_ty, f64_ty, f64_ty])
    }

    /// A live `view` `{ _0: box, _1: cursor }` over a box of type `box_ty`.
    pub(crate) fn collection_view_type(
        &mut self,
        ctx: &Context,
        box_ty: TypeHandle,
        view: CollectionViewKind,
    ) -> Result<TypedHandle<RecordType>> {
        let cursor_rec = self.cursor_tuple_type(ctx)?;
        self.positional_record(ctx, view.record_kind(), [box_ty, cursor_rec.into()])
    }

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
        let collection =
            CollectionKind::from_type_name(name).or_else(|| view.map(CollectionViewKind::source));
        if let Some(collection) = collection {
            let [key, value] = collection.type_fields(arguments).ok_or_else(|| {
                let needed = match collection {
                    CollectionKind::Map => "key and value type arguments",
                    CollectionKind::Set => "one element type argument",
                };
                diag!(codes::TYPE_MISMATCH, "{name} needs {needed}")
            })?;
            let key = carrier(self, &key)?;
            let entries = match collection {
                CollectionKind::Map => EntryTypes::Map(key, carrier(self, &value)?),
                CollectionKind::Set => EntryTypes::Set(key),
            };
            let storage = self.collection_type(ctx, entries)?;
            return Ok(Some(match view {
                Some(view) => self.collection_view_type(ctx, storage.into(), view)?.into(),
                None => storage.into(),
            }));
        }
        Ok(Some(match (name, arguments) {
            ("WitInput", [arg]) => self.project_view(ctx, *arg, visiting, true)?,
            ("PromiseSettledResult", [arg]) => {
                perry_mir::SettledType::get(ctx, self.project_view(ctx, *arg, visiting, false)?)
                    .into()
            }
            ("ReadonlyArray" | "Array", [arg]) => ArrayType::get(ctx, carrier(self, arg)?).into(),
            ("Readonly", [arg]) => carrier(self, arg)?,
            ("Record", [key, value]) => {
                let key = carrier(self, key)?;
                let value = carrier(self, value)?;
                let entry =
                    self.positional_record(ctx, RecordKind::DictionaryEntry, [key, value])?;
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
