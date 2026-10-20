//! Keyed operations of `Map`, `Set` and `Record<K, V>` dictionaries, and their iterators.

use super::*;
use crate::coerce::admitting;
use crate::control_flow::Input;

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

impl<'s> FunctionBuilder<'s> {
    fn canonicalize_collection_key(&mut self, ctx: &mut Context, key: Value) -> Result<Value> {
        let ty = key.get_type(ctx);
        if !matches!(
            Kind::of(ctx, present_type(ctx, ty)),
            Some(Kind::Number | Kind::NumberString)
        ) {
            return Ok(key);
        }
        let zero = self.number(ctx, 0.0);
        let zero = self.fit(ctx, zero, ty)?;
        let is_zero = self.compare(ctx, CompareKind::Eq, key, zero);
        self.choose(ctx, ty, is_zero, |_, _| Ok(zero), |_, _| Ok(key))
    }

    /// Run `hit(index, entry)` on the first live entry of `entries` whose key is
    /// SameValueZero-equal to `key`, then leave the scan.
    fn on_live_match(
        &mut self,
        ctx: &mut Context,
        label: &str,
        entries: Value,
        key: Value,
        key_ty: TypeHandle,
        hit: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<()>,
    ) -> Result<()> {
        self.iterate(
            ctx,
            Input::Collection(entries),
            |b, ctx, entry, idx, next, done| {
                let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
                let cand_k = b.append_value(ctx, &k_op);
                let matched = b.same_value_zero(ctx, cand_k, key, key_ty)?;
                let hit_bb = b.new_block(ctx, &format!("{label}_hit"));
                b.branch(ctx, matched, hit_bb, next);
                b.switch_to(hit_bb);
                hit(b, ctx, idx, entry)?;
                b.jump(ctx, done);
                Ok(())
            },
        )
    }

    /// [Self::on_live_match], yielding whether an entry matched.
    fn find_live_match(
        &mut self,
        ctx: &mut Context,
        label: &str,
        entries: Value,
        key: Value,
        key_ty: TypeHandle,
        hit: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<()>,
    ) -> Result<Value> {
        let bool_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let found_slot = self.entry_slot(ctx, bool_ty);
        let false_val = self.boolean(ctx, false);
        self.store(ctx, found_slot, false_val);
        self.on_live_match(ctx, label, entries, key, key_ty, |b, ctx, idx, entry| {
            hit(b, ctx, idx, entry)?;
            let true_val = b.boolean(ctx, true);
            b.store(ctx, found_slot, true_val);
            Ok(())
        })?;
        self.load(ctx, found_slot)
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
        let result = self.entry_slot(ctx, opt_v_ty);
        self.store(ctx, result, miss);
        self.on_live_match(ctx, "kv_get", arr, key, k_ty, |b, ctx, _, entry| {
            let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
            let cand_v = b.append_value(ctx, &v_op);
            let wrapped = b.fit(ctx, cand_v, opt_v_ty)?;
            b.store(ctx, result, wrapped);
            Ok(())
        })?;
        self.load(ctx, result)
    }

    /// Insert or overwrite `(key, val)` in a `Map<K, V>` or `Record<K, V>`.
    pub(crate) fn kv_set(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        val: Value,
    ) -> Result<Value> {
        let (k_ty, v_ty, _) = kv_entry_types(ctx, coll.get_type(ctx))
            .context("key-value insertion requires a Map, dictionary or entry array")?;
        let arr = self.collection_entries_array(ctx, coll)?;
        let box_opt = self.collection_underlying_box(ctx, coll)?;
        let key = self.fit(ctx, key, k_ty)?;
        let key = self.canonicalize_collection_key(ctx, key)?;
        let val = self.fit(ctx, val, v_ty)?;
        let was_found =
            self.find_live_match(ctx, "kv_set", arr, key, k_ty, |b, ctx, _, entry| {
                b.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, entry, "_0".to_string(), key)
                });
                b.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, entry, "_1".to_string(), val)
                });
                Ok(())
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
        let record = TypedHandle::<RecordType>::from_handle(self.array_element(ctx, entries)?, ctx)
            .context("collection entries must be records")?;
        let mut fields = Vec::with_capacity(record.deref(ctx).fields()?.len());
        fields.extend_from_slice(payload);
        let append_bb = self.new_block(ctx, &format!("{label}_append"));
        let after_bb = self.new_block(ctx, &format!("{label}_after"));
        self.branch(ctx, present, after_bb, append_bb);
        self.switch_to(append_bb);
        if let Some(box_val) = owner {
            fields.push(self.boolean(ctx, true));
            fields.push(self.next_collection_seq(ctx, box_val)?);
        }
        let entry = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, fields));
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, entries, entry));
        if let Some(box_val) = owner {
            self.bump_live_count(ctx, box_val, BinaryKind::Add)?;
        }
        self.jump(ctx, after_bb);
        self.switch_to(after_bb);
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
        self.find_live_match(ctx, "kv_has", arr, key, k_ty, |_, _, _, _| Ok(()))
    }

    /// `delete(key)` on a `Map` tombstones the entry; on a dictionary it compacts in place.
    pub(crate) fn kv_delete(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        k_ty: TypeHandle,
    ) -> Result<Value> {
        let key = self.fit(ctx, key, k_ty)?;
        if let Some((_, v_ty, _)) = map_entry_types(ctx, coll.get_type(ctx)) {
            let arr = self.collection_entries_array(ctx, coll)?;
            return self.find_live_match(ctx, "map_delete", arr, key, k_ty, |b, ctx, _, entry| {
                let cleared = [("_0", k_ty), ("_1", v_ty)];
                b.tombstone_entry(ctx, arr, entry, EntryKind::Map, &cleared, coll)
            });
        }
        self.retain_array(ctx, coll, |b, ctx, entry| {
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let candidate = b.append_value(ctx, &k_op);
            let matched = b.same_value_zero(ctx, candidate, key, k_ty)?;
            Ok(b.not(ctx, matched))
        })
    }

    /// `forEach` passes `(value, key, collection)`; a Set's key equals its value.
    pub(crate) fn collection_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        let is_set = set_box_record(ctx, arr.get_type(ctx)).is_some();
        self.iterate(ctx, Input::Collection(arr), |b, ctx, entry, _, _, _| {
            let (k, v) = if is_set {
                (entry, entry)
            } else {
                b.entry_fields(ctx, entry)?
            };
            b.array_callback(ctx, callback, &[v, k, arr])?;
            Ok(())
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
        let present =
            self.find_live_match(ctx, "set_has", arr, val, elem_ty, |_, _, _, _| Ok(()))?;
        self.append_unless(ctx, "set_add", present, arr, Some(coll), &[val])?;
        Ok(coll)
    }

    pub(crate) fn set_delete(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        val: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let arr = self.collection_entries_array(ctx, coll)?;
        let val = self.fit(ctx, val, elem_ty)?;
        self.find_live_match(ctx, "set_delete", arr, val, elem_ty, |b, ctx, _, entry| {
            b.tombstone_entry(ctx, arr, entry, EntryKind::Set, &[("_0", elem_ty)], coll)
        })
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
        let out = self.empty_set(ctx, Some(receiver.get_type(ctx)))?;
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
        let i1 = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let res_slot = self.entry_slot(ctx, i1);
        let init_true = self.boolean(ctx, true);
        self.store(ctx, res_slot, init_true);
        let (iter_set, check_set, negate) = match relation {
            SetRelation::Subset => (receiver, other, false),
            SetRelation::Superset => (other, receiver, false),
            SetRelation::Disjoint => (receiver, other, true),
        };
        self.iterate(
            ctx,
            Input::Collection(iter_set),
            |b, ctx, val, _, next, done| {
                let has = b.kv_has(ctx, check_set, val, elem_ty)?;
                let bad = if negate { has } else { b.not(ctx, has) };
                let fail_bb = b.new_block(ctx, "set_rel_fail");
                b.branch(ctx, bad, fail_bb, next);
                b.switch_to(fail_bb);
                let f = b.boolean(ctx, false);
                b.store(ctx, res_slot, f);
                b.jump(ctx, done);
                Ok(())
            },
        )?;
        self.load(ctx, res_slot)
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
                    None => b.shapes.map_tuple_type(ctx, k_ty, group_arr_ty)?,
                };

            let body_bb = b.block;
            b.switch_to(alloc_bb);
            let out_map = b.alloc_collection_box(ctx, tuple_rec, EntryKind::Map)?;
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
