//! Keyed operations of `Map`, `Set` and `Record<K, V>` dictionaries, and their iterators.

use super::StorageField as Field;
use super::*;
use crate::lower::coerce::admitting;
use crate::lower::control_flow::Input;

pub(crate) enum SetOperation {
    Difference,
    Intersection,
    Union,
    SymmetricDifference,
}
pub(crate) enum SetRelation {
    Subset,
    Superset,
    Disjoint,
}

#[derive(Clone, Copy)]
enum SetSelection {
    All,
    PresentIn(Value),
    AbsentFrom(Value),
}

impl<'s> FunctionBuilder<'s, '_> {
    fn canonicalize_collection_key(&mut self, ctx: &mut Context, key: Value) -> Result<Value> {
        let ty = key.get_type(ctx);
        let numeric = match Kind::of(ctx, present_type(ctx, ty)) {
            Some(Kind::Number) => true,
            Some(Kind::ValueUnion) => perry_mir::VariantType::of(ctx, present_type(ctx, ty))
                .to_diag(ctx)?
                .case("number")
                .is_some(),
            _ => false,
        };
        if !numeric {
            return Ok(key);
        }
        let zero = self.number(ctx, 0.0);
        let zero = self.fit(ctx, zero, ty)?;
        let is_zero = self.compare(ctx, CompareKind::Eq, key, zero);
        self.choose(ctx, ty, is_zero, |_, _| Ok(zero), |_, _| Ok(key))
    }

    /// Return `miss` or the result of `hit` on the first live SameValueZero match.
    fn lookup_entry(
        &mut self,
        ctx: &mut Context,
        entries: Value,
        key: Value,
        miss: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        hit: impl FnOnce(&mut Self, &mut Context, Value) -> Result<Value>,
    ) -> Result<Value> {
        self.find_item(
            ctx,
            Input::Collection(entries),
            miss,
            |b, ctx, entry, _| {
                let candidate = b.storage_field(ctx, entry, Field::Key)?;
                b.same_value(
                    ctx,
                    candidate,
                    key,
                    crate::lower::operators::Equality::SameValueZero,
                )
            },
            |b, ctx, entry, _| hit(b, ctx, entry),
        )
    }

    /// `get(key)` on a `Map` or dictionary: the value, or `undefined`.
    pub(crate) fn kv_get(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        k_ty: TypeHandle,
        v_ty: TypeHandle,
    ) -> Result<Value> {
        let arr = self.collection_entries_array(ctx, coll)?;
        let key = self.fit(ctx, key, k_ty)?;
        let opt_v_ty = admitting(ctx, v_ty, perry_mir::Absence::Undefined);
        self.lookup_entry(
            ctx,
            arr,
            key,
            |b, ctx| Ok(b.emit_value(UndefinedOp::with_type(ctx, opt_v_ty), ctx)),
            |b, ctx, entry| {
                let value = b.storage_field(ctx, entry, Field::MapValue)?;
                b.fit(ctx, value, opt_v_ty)
            },
        )
    }

    /// The private handle table only inserts fresh keys. Its insertion sequence survives deletions.
    pub(in crate::lower) fn insert_resource(
        &mut self,
        ctx: &mut Context,
        table: Value,
        instance: Value,
    ) -> Result<Value> {
        let sequence = self.record_field(ctx, table, StorageField::BoxSequence.name())?;
        let one = self.number(ctx, 1.0);
        let rep = self.binary(ctx, BinaryKind::Add, sequence, one);
        let (_, ty) = self
            .shapes
            .key_value_types(ctx, table.get_type(ctx))
            .ok_or_else(|| internal!("invalid resource table"))?;
        let value = self.fit(ctx, instance, ty)?;
        let entries = self.collection_entries_array(ctx, table)?;
        self.append_entry(ctx, entries, Some(table), &[rep, value])?;
        Ok(rep)
    }

    /// Insert or overwrite `(key, val)` in a `Map<K, V>` or `Record<K, V>`.
    pub(crate) fn kv_set(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        val: Value,
    ) -> Result<Value> {
        let (k_ty, v_ty) = self
            .shapes
            .key_value_types(ctx, coll.get_type(ctx))
            .ok_or_else(|| {
                Diagnostic::new(
                    codes::TYPE_MISMATCH,
                    "key-value insertion requires a Map, dictionary or entry array",
                )
            })?;
        let key = self.fit(ctx, key, k_ty)?;
        let val = self.fit(ctx, val, v_ty)?;
        self.insert_or_update(
            ctx,
            coll,
            key,
            |_, _, key| Ok([key, val]),
            |b, ctx, entry| {
                b.set_storage_field(ctx, entry, Field::MapValue, val);
                Ok(())
            },
        )
    }

    fn insert_or_update<const N: usize>(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        vacant: impl FnOnce(&mut Self, &mut Context, Value) -> Result<[Value; N]>,
        occupied: impl FnOnce(&mut Self, &mut Context, Value) -> Result<()>,
    ) -> Result<Value> {
        let (entries, owner) = match self.collection_storage(ctx, coll)? {
            Some((_, owner)) => (
                self.storage_field(ctx, owner, Field::BoxEntries)?,
                Some(owner),
            ),
            None => (coll, None),
        };
        let key = self.canonicalize_collection_key(ctx, key)?;
        self.lookup_entry(
            ctx,
            entries,
            key,
            |b, ctx| {
                let fields = vacant(b, ctx, key)?;
                b.append_entry(ctx, entries, owner, &fields)?;
                Ok(coll)
            },
            |b, ctx, entry| {
                occupied(b, ctx, entry)?;
                Ok(coll)
            },
        )?;
        Ok(coll)
    }

    fn append_entry(
        &mut self,
        ctx: &mut Context,
        entries: Value,
        owner: Option<Value>,
        payload: &[Value],
    ) -> Result<()> {
        let record = TypedHandle::<RecordType>::from_handle(self.array_element(ctx, entries)?, ctx)
            .map_err(|_| Diagnostic::internal("collection entries must be records"))?;
        let mut fields = Vec::with_capacity(record.deref(ctx).fields().to_diag(ctx)?.len());
        fields.extend_from_slice(payload);
        if let Some(box_val) = owner {
            fields.push(self.boolean(ctx, true));
            let (sequence, _) =
                self.update_counter(ctx, box_val, Field::BoxSequence, BinaryKind::Add)?;
            fields.push(sequence);
        }
        let entry = self.emit_value(AllocOp::new(ctx, record, fields), ctx);
        self.emit(ArrayPushOp::new(ctx, entries, entry), ctx);
        if let Some(box_val) = owner {
            self.update_counter(ctx, box_val, Field::BoxLength, BinaryKind::Add)?;
        }
        Ok(())
    }

    /// Membership in a Map, Set or dictionary.
    pub(crate) fn kv_has(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        k_ty: TypeHandle,
    ) -> Result<Value> {
        let arr = self.collection_entries_array(ctx, coll)?;
        let key = self.fit(ctx, key, k_ty)?;
        self.lookup_entry(
            ctx,
            arr,
            key,
            |b, ctx| Ok(b.boolean(ctx, false)),
            |b, ctx, _| Ok(b.boolean(ctx, true)),
        )
    }

    /// Lower `delete(key)`, tombstoning the entry on a `Map` or `Set` or compacting in place on a dictionary.
    pub(crate) fn kv_delete(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        k_ty: TypeHandle,
    ) -> Result<Value> {
        let key = self.fit(ctx, key, k_ty)?;
        if let Some(layout) = self.shapes.collection_box(ctx, coll.get_type(ctx)) {
            let kind = layout.view.source();
            let entries = self.storage_field(ctx, coll, Field::BoxEntries)?;
            return self.lookup_entry(
                ctx,
                entries,
                key,
                |b, ctx| Ok(b.boolean(ctx, false)),
                |b, ctx, entry| {
                    b.delete_entry(ctx, coll, entries, entry, kind)?;
                    Ok(b.boolean(ctx, true))
                },
            );
        }
        self.retain_array(ctx, coll, |b, ctx, entry| {
            let candidate = b.storage_field(ctx, entry, Field::Key)?;
            let matched = b.same_value(
                ctx,
                candidate,
                key,
                crate::lower::operators::Equality::SameValueZero,
            )?;
            Ok(b.not(ctx, matched))
        })
    }

    pub(crate) fn set_add(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        val: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let key = self.fit(ctx, val, elem_ty)?;
        self.insert_or_update(ctx, coll, key, |_, _, key| Ok([key]), |_, _, _| Ok(()))
    }

    pub(crate) fn set_operation(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        other_expr: &'s Expr,
        operation: SetOperation,
    ) -> Result<Value> {
        let other = self.expression(ctx, other_expr)?;
        let other = self.defined(ctx, other)?;
        let layout = self.set_layout(ctx, receiver)?;
        let elem_ty = layout.fields[0];
        let out = self.alloc_collection_box(ctx, layout);
        let (first, second) = match operation {
            SetOperation::Difference => (SetSelection::AbsentFrom(other), None),
            SetOperation::Intersection => {
                let receiver_size = self.collection_size(ctx, receiver)?;
                let other_size = self.collection_size(ctx, other)?;
                let receiver_first = self.compare(ctx, CompareKind::Le, receiver_size, other_size);
                let intersection = |b: &mut Self, ctx: &mut Context, source, other| {
                    b.copy_set_entries(ctx, source, SetSelection::PresentIn(other), out, elem_ty)?;
                    Ok(out)
                };
                return self.choose(
                    ctx,
                    out.get_type(ctx),
                    receiver_first,
                    |b, ctx| intersection(b, ctx, receiver, other),
                    |b, ctx| intersection(b, ctx, other, receiver),
                );
            }
            SetOperation::Union => (SetSelection::All, Some((other, SetSelection::All))),
            SetOperation::SymmetricDifference => (
                SetSelection::AbsentFrom(other),
                Some((other, SetSelection::AbsentFrom(receiver))),
            ),
        };
        for (source, selection) in std::iter::once((receiver, first)).chain(second) {
            self.copy_set_entries(ctx, source, selection, out, elem_ty)?;
        }
        Ok(out)
    }

    fn copy_set_entries(
        &mut self,
        ctx: &mut Context,
        source: Value,
        selection: SetSelection,
        out: Value,
        elem_ty: TypeHandle,
    ) -> Result<()> {
        self.iterate(ctx, Input::Collection(source), |b, ctx, val, _, next, _| {
            let val = val.into_value(b, ctx)?;
            if let SetSelection::PresentIn(set) | SetSelection::AbsentFrom(set) = selection {
                let present = b.kv_has(ctx, set, val, elem_ty)?;
                let keep = if matches!(selection, SetSelection::AbsentFrom(_)) {
                    b.not(ctx, present)
                } else {
                    present
                };
                let add = b.new_block(ctx, "set_copy_add");
                b.branch(ctx, keep, add, next);
                b.switch_to(add);
            }
            b.set_add(ctx, out, val, elem_ty)?;
            Ok(())
        })
    }

    pub(crate) fn set_relation(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        other_expr: &'s Expr,
        relation: SetRelation,
    ) -> Result<Value> {
        let other = self.expression(ctx, other_expr)?;
        let other = self.defined(ctx, other)?;
        let elem_ty = self.set_layout(ctx, receiver)?.fields[0];
        let (iter_set, check_set, negate) = match relation {
            SetRelation::Subset => (receiver, other, false),
            SetRelation::Superset => (other, receiver, false),
            SetRelation::Disjoint => (receiver, other, true),
        };
        self.find_item(
            ctx,
            Input::Collection(iter_set),
            |b, ctx| Ok(b.boolean(ctx, true)),
            |b, ctx, val, _| {
                let has = b.kv_has(ctx, check_set, val, elem_ty)?;
                Ok(if negate { has } else { b.not(ctx, has) })
            },
            |b, ctx, _, _| Ok(b.boolean(ctx, false)),
        )
    }

    pub(super) fn map_group_by(
        &mut self,
        ctx: &mut Context,
        items_expr: &'s Expr,
        callback: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let items = self.expression(ctx, items_expr)?;
        let items = self.defined(ctx, items)?;
        let callback = self.prepare_callback(ctx, callback)?;
        let ordinal = self.callback_ordinal(ctx);
        let input = if items.get_type(ctx).deref(ctx).is::<StringType>() {
            Input::String(items)
        } else {
            Input::Collection(items)
        };

        let alloc_bb = self.new_block(ctx, "groupby_alloc");
        let loop_start_bb = self.new_block(ctx, "groupby_start");
        self.jump(ctx, alloc_bb);
        self.switch_to(loop_start_bb);

        let declared =
            expected.and_then(|ty| self.shapes.boxed_collection(ctx, ty, CollectionKind::Map));
        self.iterate(ctx, input, |b, ctx, elem, _, _, _| {
            let elem = elem.into_value(b, ctx)?;
            let elem_ty = elem.get_type(ctx);
            let group_arr_ty = ArrayType::get(ctx, elem_ty).into();
            let raw_key = b.indexed_callback(ctx, callback, ordinal, elem)?;
            let layout = match declared {
                Some(map) => map,
                None => b
                    .shapes
                    .collection_type(ctx, EntryTypes::Map(raw_key.get_type(ctx), group_arr_ty))?,
            };
            let k_ty = layout.fields[0];
            let key = b.fit(ctx, raw_key, k_ty)?;
            let body_bb = b.block;
            b.switch_to(alloc_bb);
            let out_map = b.alloc_collection_box(ctx, layout);
            b.jump(ctx, loop_start_bb);
            b.switch_to(body_bb);

            b.insert_or_update(
                ctx,
                out_map,
                key,
                |b, ctx, key| {
                    let group = b.emit_value(ArrayNewOp::new(ctx, elem_ty, vec![elem]), ctx);
                    Ok([key, b.fit(ctx, group, layout.fields[1])?])
                },
                |b, ctx, entry| {
                    let group = b.storage_field(ctx, entry, Field::MapValue)?;
                    let group = b.defined(ctx, group)?;
                    let item_type = b.array_element(ctx, group)?;
                    let item = b.fit(ctx, elem, item_type)?;
                    b.emit(ArrayPushOp::new(ctx, group, item), ctx);
                    Ok(())
                },
            )
        })
    }
}
