//! MIR records of `Map`, `Set`, their live views, dictionaries and pairs.
use super::*;
use crate::lower::collections::{CollectionKind, CollectionLayout, CollectionViewKind, EntryTypes};

impl<T> Shapes<'_, T> {
    pub(in crate::lower) fn collection_type(
        &mut self,
        ctx: &Context,
        types: EntryTypes,
    ) -> Result<CollectionLayout> {
        let live = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let number = FP64Type::get(ctx).into();
        let (kind, fields, entry) = match types {
            EntryTypes::Map(key, value) => (
                CollectionKind::Map,
                [key, value],
                self.positional_record(ctx, RecordKind::MapEntry, [key, value, live, number])?,
            ),
            EntryTypes::Set(item) => (
                CollectionKind::Set,
                [item; 2],
                self.positional_record(ctx, RecordKind::SetEntry, [item, live, number])?,
            ),
        };
        let entries = ArrayType::get(ctx, entry.into()).into();
        let boxed =
            self.positional_record(ctx, kind.box_kind(), [entries, number, number, number])?;
        Ok(CollectionLayout {
            view: kind.default_view(),
            boxed,
            entry,
            fields,
        })
    }

    /// The `{ _0: index, _1: min_seq, _2: epoch }` cursor of a live iterator.
    pub(crate) fn cursor_tuple_type(&mut self, ctx: &Context) -> Result<TypedHandle<RecordType>> {
        let f64_ty: TypeHandle = FP64Type::get(ctx).into();
        self.positional_record(ctx, RecordKind::Tuple, [f64_ty; 3])
    }

    pub(in crate::lower) fn collection_view_type(
        &mut self,
        ctx: &Context,
        layout: CollectionLayout,
        view: CollectionViewKind,
    ) -> Result<TypedHandle<RecordType>> {
        let cursor = self.cursor_tuple_type(ctx)?;
        self.positional_record(
            ctx,
            view.record_kind(),
            [layout.boxed.into(), cursor.into()],
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
        let collection =
            CollectionKind::from_type_name(name).or_else(|| view.map(CollectionViewKind::source));
        if let Some(collection) = collection {
            let entry = collection.entry(arguments).ok_or_else(|| {
                let needed = match collection {
                    CollectionKind::Map => "key and value type arguments",
                    CollectionKind::Set => "one element type argument",
                };
                diag!(codes::TYPE_MISMATCH, "{name} needs {needed}")
            })?;
            let [key, value] = entry.key_value();
            let key = carrier(self, &key)?;
            let entries = match collection {
                CollectionKind::Map => EntryTypes::Map(key, carrier(self, &value)?),
                CollectionKind::Set => EntryTypes::Set(key),
            };
            let storage = self.collection_type(ctx, entries)?;
            return Ok(Some(match view {
                Some(view) => self.collection_view_type(ctx, storage, view)?.into(),
                None => storage.boxed.into(),
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
