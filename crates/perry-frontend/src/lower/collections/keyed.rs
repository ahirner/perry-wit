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
            Some(Kind::ValueUnion) => perry_mir::variant_cases(ctx, present_type(ctx, ty))
                .to_diag(ctx)?
                .iter()
                .any(|(tag, _)| tag == "number"),
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
        miss: Value,
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
        let miss = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_v_ty));
        self.lookup_entry(ctx, arr, key, miss, |b, ctx, entry| {
            let value = b.storage_field(ctx, entry, Field::MapValue)?;
            b.fit(ctx, value, opt_v_ty)
        })
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
        let (_, ty, _) = map_entry_types(ctx, table.get_type(ctx))
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
        let (k_ty, v_ty, _) = kv_entry_types(ctx, coll.get_type(ctx)).ok_or_else(|| {
            Diagnostic::new(
                codes::TYPE_MISMATCH,
                "key-value insertion requires a Map, dictionary or entry array",
            )
        })?;
        let arr = self.collection_entries_array(ctx, coll)?;
        let box_opt = self
            .collection_storage(ctx, coll)?
            .map(|(_, storage)| storage);
        let key = self.fit(ctx, key, k_ty)?;
        let key = self.canonicalize_collection_key(ctx, key)?;
        let val = self.fit(ctx, val, v_ty)?;
        let miss = self.boolean(ctx, false);
        let was_found = self.lookup_entry(ctx, arr, key, miss, |b, ctx, entry| {
            b.set_storage_field(ctx, entry, Field::Key, key);
            b.set_storage_field(ctx, entry, Field::MapValue, val);
            Ok(b.boolean(ctx, true))
        })?;
        self.append_unless(ctx, "kv_set", was_found, arr, box_opt, &[key, val])?;
        Ok(coll)
    }

    /// Append an absent key, adding liveness and sequence fields for boxed collections.
    fn append_unless(
        &mut self,
        ctx: &mut Context,
        label: &str,
        present: Value,
        entries: Value,
        owner: Option<Value>,
        payload: &[Value],
    ) -> Result<()> {
        let append_bb = self.new_block(ctx, &format!("{label}_append"));
        let after_bb = self.new_block(ctx, &format!("{label}_after"));
        self.branch(ctx, present, after_bb, append_bb);
        self.switch_to(append_bb);
        self.append_entry(ctx, entries, owner, payload)?;
        self.jump(ctx, after_bb);
        self.switch_to(after_bb);
        Ok(())
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
        let entry = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, fields));
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, entries, entry));
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
        self.contains_entry(ctx, arr, key)
    }

    fn contains_entry(&mut self, ctx: &mut Context, entries: Value, key: Value) -> Result<Value> {
        let miss = self.boolean(ctx, false);
        self.lookup_entry(
            ctx,
            entries,
            key,
            miss,
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
        if let Some(Collection::Box(kind)) = Collection::of(ctx, coll.get_type(ctx)) {
            let entries = self.storage_field(ctx, coll, Field::BoxEntries)?;
            let miss = self.boolean(ctx, false);
            return self.lookup_entry(ctx, entries, key, miss, |b, ctx, entry| {
                b.delete_entry(ctx, coll, entries, entry, kind)?;
                Ok(b.boolean(ctx, true))
            });
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

    /// Lower `forEach`, passing `(value, key, collection)` with `key` equal to `value` on a `Set`.
    pub(crate) fn collection_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        let callback = self.prepare_callback(ctx, callback)?;
        let is_set = matches!(
            Collection::of(ctx, arr.get_type(ctx)),
            Some(Collection::Box(CollectionKind::Set))
        );
        self.iterate(ctx, Input::Collection(arr), |b, ctx, entry, _, _, _| {
            let (k, v) = if is_set {
                (entry, entry)
            } else {
                b.entry_fields(ctx, entry)?
            };
            b.effect_callback(ctx, callback, &[v, k, arr])
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    pub(crate) fn set_add(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        val: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let arr = self.collection_entries_array(ctx, coll)?;
        let val = self.fit(ctx, val, elem_ty)?;
        let val = self.canonicalize_collection_key(ctx, val)?;
        let present = self.contains_entry(ctx, arr, val)?;
        self.append_unless(ctx, "set_add", present, arr, Some(coll), &[val])?;
        Ok(coll)
    }

    pub(crate) fn set_binary_op(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        other_expr: &'s Expr,
        elem_ty: TypeHandle,
        operation: SetOperation,
    ) -> Result<Value> {
        let other = self.expression(ctx, other_expr)?;
        let other = self.defined(ctx, other)?;
        let out = self.empty_collection(ctx, CollectionKind::Set, Some(receiver.get_type(ctx)))?;
        let (first, second) = match operation {
            SetOperation::Difference => (SetSelection::AbsentFrom(other), None),
            SetOperation::Intersection => {
                let receiver_size = self.collection_size(ctx, receiver)?;
                let other_size = self.collection_size(ctx, other)?;
                let receiver_first = self.compare(ctx, CompareKind::Le, receiver_size, other_size);
                return self.choose(
                    ctx,
                    out.get_type(ctx),
                    receiver_first,
                    |b, ctx| {
                        b.copy_set_entries(
                            ctx,
                            receiver,
                            SetSelection::PresentIn(other),
                            out,
                            elem_ty,
                        )?;
                        Ok(out)
                    },
                    |b, ctx| {
                        b.copy_set_entries(
                            ctx,
                            other,
                            SetSelection::PresentIn(receiver),
                            out,
                            elem_ty,
                        )?;
                        Ok(out)
                    },
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

    pub(crate) fn set_relation_op(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        other_expr: &'s Expr,
        elem_ty: TypeHandle,
        relation: SetRelation,
    ) -> Result<Value> {
        let other = self.expression(ctx, other_expr)?;
        let other = self.defined(ctx, other)?;
        let all_match = self.boolean(ctx, true);
        let (iter_set, check_set, negate) = match relation {
            SetRelation::Subset => (receiver, other, false),
            SetRelation::Superset => (other, receiver, false),
            SetRelation::Disjoint => (receiver, other, true),
        };
        self.find_item(
            ctx,
            Input::Collection(iter_set),
            all_match,
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
        let elem_ty = self.array_element(ctx, items)?;
        let group_arr_ty: TypeHandle = ArrayType::get(ctx, elem_ty).into();

        let alloc_bb = self.new_block(ctx, "groupby_alloc");
        let loop_start_bb = self.new_block(ctx, "groupby_start");
        self.jump(ctx, alloc_bb);
        self.switch_to(loop_start_bb);

        self.iterate(ctx, Input::Collection(items), |b, ctx, elem, idx, _, _| {
            let raw_key = b.array_callback(ctx, callback, &[elem, idx])?;
            let k_ty = expected
                .and_then(|exp| map_entry_types(ctx, exp).map(|(k, _, _)| k))
                .unwrap_or_else(|| raw_key.get_type(ctx));
            let key = b.fit(ctx, raw_key, k_ty)?;
            let tuple_rec =
                match expected.and_then(|exp| map_entry_types(ctx, exp).map(|(_, _, r)| r)) {
                    Some(r) => r,
                    None => b
                        .shapes
                        .entry_type(ctx, EntryTypes::Map(k_ty, group_arr_ty))?,
                };

            let body_bb = b.block;
            b.switch_to(alloc_bb);
            let out_map = b.alloc_collection_box(ctx, tuple_rec, CollectionKind::Map)?;
            b.jump(ctx, loop_start_bb);
            b.switch_to(body_bb);

            let existing_opt = b.kv_get(ctx, out_map, key, k_ty, group_arr_ty)?;
            let is_undef = b.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, existing_opt));
            let new_bb = b.new_block(ctx, "groupby_new");
            let push_bb = b.new_block(ctx, "groupby_push");
            let done_bb = b.new_block(ctx, "groupby_done");
            b.branch(ctx, is_undef, new_bb, push_bb);

            b.switch_to(new_bb);
            let fresh_arr = b.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![elem]));
            b.kv_set(ctx, out_map, key, fresh_arr)?;
            b.jump(ctx, done_bb);

            b.switch_to(push_bb);
            let existing_arr = b.defined(ctx, existing_opt)?;
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, existing_arr, elem));
            b.jump(ctx, done_bb);

            b.switch_to(done_bb);
            Ok(out_map)
        })
    }
}
