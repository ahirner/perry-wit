//! Keyed operations of `Map`, `Set` and `Record<K, V>` dictionaries, and their iterators.

use super::*;
use crate::coerce::admitting;

#[derive(Clone, Copy)]
enum SetSelection {
    All,
    PresentIn(Value),
    AbsentFrom(Value),
}

impl<'s> FunctionBuilder<'s> {
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
        let limit = self.array_length(ctx, entries);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, label, zero, limit, |b, ctx, idx, next, done| {
            let entry = b.element_at(ctx, entries, idx)?;
            if let Some(live) = b.collection_entry_live(ctx, entry)? {
                let live_bb = b.new_block(ctx, &format!("{label}_live"));
                b.branch(ctx, live, live_bb, next);
                b.switch_to(live_bb);
            }
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let cand_k = b.append_value(ctx, &k_op);
            let matched = b.same_value_zero(ctx, cand_k, key, key_ty)?;
            let hit_bb = b.new_block(ctx, &format!("{label}_hit"));
            b.branch(ctx, matched, hit_bb, next);
            b.switch_to(hit_bb);
            hit(b, ctx, idx, entry)?;
            b.jump(ctx, done);
            Ok(())
        })
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
        let (k_ty, v_ty, tuple_rec) = kv_entry_types(ctx, coll.get_type(ctx))
            .context("key-value insertion requires a Map, dictionary or entry array")?;
        let arr = self.collection_entries_array(ctx, coll)?;
        let box_opt = self.collection_underlying_box(ctx, coll)?;
        let key = self.fit(ctx, key, k_ty)?;
        let val = self.fit(ctx, val, v_ty)?;
        let is_map = EntryKind::of_entry(ctx, tuple_rec) == Some(EntryKind::Map);
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
        self.append_unless(ctx, "kv_set", was_found, arr, box_opt, |b, ctx| {
            if is_map {
                let live_true = b.boolean(ctx, true);
                let seq = b.next_collection_seq(ctx, box_opt)?;
                Ok(b.emit_value(ctx, |ctx| {
                    AllocOp::new(ctx, tuple_rec, vec![key, val, live_true, seq])
                }))
            } else {
                Ok(b.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![key, val])))
            }
        })?;
        Ok(coll)
    }

    /// Unless `present`, push the entry `make_entry` builds onto `entries` and count it in
    /// `owner`.
    fn append_unless(
        &mut self,
        ctx: &mut Context,
        label: &str,
        present: Value,
        entries: Value,
        owner: Option<Value>,
        make_entry: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
    ) -> Result<()> {
        let append_bb = self.new_block(ctx, &format!("{label}_append"));
        let after_bb = self.new_block(ctx, &format!("{label}_after"));
        self.branch(ctx, present, after_bb, append_bb);
        self.switch_to(append_bb);
        let entry = make_entry(self, ctx)?;
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, entries, entry));
        if let Some(box_val) = owner {
            self.bump_live_count(ctx, box_val, BinaryKind::Add)?;
        }
        self.jump(ctx, after_bb);
        self.switch_to(after_bb);
        Ok(())
    }

    /// `has(key)` on a `Map`, or `key in` a dictionary.
    pub(super) fn kv_has(
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
    pub(super) fn kv_delete(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        key: Value,
        k_ty: TypeHandle,
    ) -> Result<Value> {
        let key = self.fit(ctx, key, k_ty)?;
        if let Some((_, v_ty, _)) = map_entry_types(ctx, coll.get_type(ctx)) {
            let arr = self.collection_entries_array(ctx, coll)?;
            let box_opt = self.collection_underlying_box(ctx, coll)?;
            return self.find_live_match(ctx, "map_delete", arr, key, k_ty, |b, ctx, _, entry| {
                let cleared = [("_0", k_ty), ("_1", v_ty)];
                b.tombstone_entry(ctx, arr, entry, EntryKind::Map, &cleared, box_opt)
            });
        }
        let bool_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let found_slot = self.entry_slot(ctx, bool_ty);
        let false_val = self.boolean(ctx, false);
        self.store(ctx, found_slot, false_val);
        let arr = coll;
        let num_ty = FP64Type::get(ctx).into();
        let write_slot = self.entry_slot(ctx, num_ty);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        self.store(ctx, write_slot, zero);
        let limit = self.array_length(ctx, arr);
        self.index_loop(ctx, "kv_delete", zero, limit, |b, ctx, idx, next, _| {
            let entry = b.element_at(ctx, arr, idx)?;
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let cand_k = b.append_value(ctx, &k_op);
            let matched = b.same_value_zero(ctx, cand_k, key, k_ty)?;
            let hit = b.new_block(ctx, "kv_delete_hit");
            let keep = b.new_block(ctx, "kv_delete_keep");
            b.branch(ctx, matched, hit, keep);
            b.switch_to(hit);
            let true_val = b.boolean(ctx, true);
            b.store(ctx, found_slot, true_val);
            b.jump(ctx, next);
            b.switch_to(keep);
            let w = b.load(ctx, write_slot)?;
            b.emit(ctx, |ctx| ArraySetOp::new(ctx, arr, w, entry));
            let next_w = b.binary(ctx, BinaryKind::Add, w, one);
            b.store(ctx, write_slot, next_w);
            Ok(())
        })?;
        let final_w = self.load(ctx, write_slot)?;
        let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let new_len = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, final_w, u32_ty));
        self.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, arr, new_len));
        self.load(ctx, found_slot)
    }

    /// `map.forEach(callback)`, called as `callback(value, key, map)`.
    pub(super) fn map_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        self.live_collection_loop(ctx, "map_foreach", arr, |b, ctx, _, entry| {
            let (k, v) = b.entry_fields(ctx, entry)?;
            b.array_callback(ctx, callback, &[v, k, arr])?;
            Ok(())
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    pub(super) fn set_add(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        val: Value,
        elem_ty: TypeHandle,
        tuple_rec: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let arr = self.collection_entries_array(ctx, coll)?;
        let box_opt = self.collection_underlying_box(ctx, coll)?;
        let val = self.fit(ctx, val, elem_ty)?;
        let present =
            self.find_live_match(ctx, "set_has", arr, val, elem_ty, |_, _, _, _| Ok(()))?;
        self.append_unless(ctx, "set_add", present, arr, box_opt, |b, ctx| {
            let live_true = b.boolean(ctx, true);
            let seq = b.next_collection_seq(ctx, box_opt)?;
            Ok(b.emit_value(ctx, |ctx| {
                AllocOp::new(ctx, tuple_rec, vec![val, live_true, seq])
            }))
        })?;
        Ok(coll)
    }

    pub(super) fn set_has(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        val: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let arr = self.collection_entries_array(ctx, coll)?;
        let val = self.fit(ctx, val, elem_ty)?;
        self.find_live_match(ctx, "set_has", arr, val, elem_ty, |_, _, _, _| Ok(()))
    }

    pub(super) fn set_delete(
        &mut self,
        ctx: &mut Context,
        coll: Value,
        val: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let arr = self.collection_entries_array(ctx, coll)?;
        let box_opt = self.collection_underlying_box(ctx, coll)?;
        let val = self.fit(ctx, val, elem_ty)?;
        self.find_live_match(ctx, "set_delete", arr, val, elem_ty, |b, ctx, _, entry| {
            b.tombstone_entry(ctx, arr, entry, EntryKind::Set, &[("_0", elem_ty)], box_opt)
        })
    }

    pub(super) fn set_binary_op(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        other_expr: &'s Expr,
        elem_ty: TypeHandle,
        tuple_rec: TypedHandle<RecordType>,
        method: &str,
    ) -> Result<Value> {
        let other = self.expression(ctx, other_expr)?;
        let other = self.defined(ctx, other)?;
        let out = self.empty_set(ctx, Some(receiver.get_type(ctx)))?;
        let (first, second) = match method {
            "difference" => (SetSelection::AbsentFrom(other), None),
            "intersection" => (SetSelection::PresentIn(other), None),
            "union" => (SetSelection::All, Some((other, SetSelection::All))),
            "symmetricDifference" => (
                SetSelection::AbsentFrom(other),
                Some((other, SetSelection::AbsentFrom(receiver))),
            ),
            _ => bail!("unsupported Set binary op {method}"),
        };
        for (source, selection) in std::iter::once((receiver, first)).chain(second) {
            self.live_collection_loop(ctx, "set_copy", source, |b, ctx, _, val| {
                let done = match selection {
                    SetSelection::All => None,
                    SetSelection::PresentIn(set) | SetSelection::AbsentFrom(set) => {
                        let present = b.set_has(ctx, set, val, elem_ty)?;
                        let keep = if matches!(selection, SetSelection::AbsentFrom(_)) {
                            b.not(ctx, present)
                        } else {
                            present
                        };
                        let add = b.new_block(ctx, "set_copy_add");
                        let done = b.new_block(ctx, "set_copy_done");
                        b.branch(ctx, keep, add, done);
                        b.switch_to(add);
                        Some(done)
                    }
                };
                b.set_add(ctx, out, val, elem_ty, tuple_rec)?;
                if let Some(done) = done {
                    b.jump(ctx, done);
                    b.switch_to(done);
                }
                Ok(())
            })?;
        }
        Ok(out)
    }

    pub(super) fn set_relation_op(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        other_expr: &'s Expr,
        elem_ty: TypeHandle,
        method: &str,
    ) -> Result<Value> {
        let other = self.expression(ctx, other_expr)?;
        let other = self.defined(ctx, other)?;
        let i1 = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let res_slot = self.entry_slot(ctx, i1);
        let init_true = self.boolean(ctx, true);
        self.store(ctx, res_slot, init_true);
        let (iter_set, check_set, negate) = match method {
            "isSubsetOf" => (receiver, other, false),
            "isSupersetOf" => (other, receiver, false),
            "isDisjointFrom" => (receiver, other, true),
            _ => bail!("unsupported Set relation op {method}"),
        };
        self.live_collection_loop(ctx, "set_rel", iter_set, |b, ctx, _, val| {
            let has = b.set_has(ctx, check_set, val, elem_ty)?;
            let bad = if negate { has } else { b.not(ctx, has) };
            let fail_bb = b.new_block(ctx, "set_rel_fail");
            let cont_bb = b.new_block(ctx, "set_rel_cont");
            b.branch(ctx, bad, fail_bb, cont_bb);
            b.switch_to(fail_bb);
            let f = b.boolean(ctx, false);
            b.store(ctx, res_slot, f);
            b.jump(ctx, cont_bb);
            b.switch_to(cont_bb);
            Ok(())
        })?;
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

        let len = self.array_length(ctx, items);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "map_groupby", zero, len, |b, ctx, idx, _, _| {
            let elem = b.element_at(ctx, items, idx)?;
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

    /// `set.values()`/`set.keys()`: a live view of a box, else a copied array.
    pub(super) fn set_values(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        if set_box_record(ctx, arr.get_type(ctx)).is_some() {
            return self.alloc_collection_view(ctx, arr, CollectionViewKind::SetValues);
        }
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![]));
        self.live_collection_loop(ctx, "set_values", arr, |b, ctx, _, item| {
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, item));
            Ok(())
        })?;
        Ok(out)
    }

    /// `set.entries()`: a live view of a box, else copied `[value, value]` pairs.
    pub(super) fn set_entries(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        if set_box_record(ctx, arr.get_type(ctx)).is_some() {
            return self.alloc_collection_view(ctx, arr, CollectionViewKind::SetEntries);
        }
        let tuple_rec = self.shapes.pair_type(ctx, elem_ty, elem_ty)?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        self.live_collection_loop(ctx, "set_entries", arr, |b, ctx, _, item| {
            let pair = b.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![item, item]));
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, pair));
            Ok(())
        })?;
        Ok(out)
    }

    /// `set.forEach(callback)`, called as `callback(value, value, set)`.
    pub(super) fn set_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        self.live_collection_loop(ctx, "set_foreach", arr, |b, ctx, _, item| {
            b.array_callback(ctx, callback, &[item, item, arr])?;
            Ok(())
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    pub(super) fn array_keys(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        let num_ty = FP64Type::get(ctx).into();
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, num_ty, vec![]));
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "arr_keys", zero, limit, |b, ctx, idx, _, _| {
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, idx));
            Ok(())
        })?;
        Ok(out)
    }

    pub(super) fn array_entries(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let num_ty = FP64Type::get(ctx).into();
        let tuple_rec = self.shapes.pair_type(ctx, num_ty, elem_ty)?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "arr_entries", zero, limit, |b, ctx, idx, _, _| {
            let item = b.element_at(ctx, arr, idx)?;
            let pair = b.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![idx, item]));
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, pair));
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn array_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        let arr = self.defined(ctx, arr)?;
        self.array_element(ctx, arr)?;
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "foreach", zero, limit, |b, ctx, idx, next, _| {
            let available = b.array_length(ctx, arr);
            let present = b.compare(ctx, CompareKind::Lt, idx, available);
            let read = b.new_block(ctx, "foreach_read");
            b.branch(ctx, present, read, next);
            b.switch_to(read);
            let item = b.element_at(ctx, arr, idx)?;
            b.array_callback(ctx, callback, &[item, idx, arr])?;
            Ok(())
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    /// `keys()` of a `Map` box (a live view) or a dictionary (a copied array).
    pub(super) fn kv_keys(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        if map_box_record(ctx, arr.get_type(ctx)).is_some() {
            return self.alloc_collection_view(ctx, arr, CollectionViewKind::MapKeys);
        }
        let (k_ty, _, _) =
            kv_entry_types(ctx, arr.get_type(ctx)).context("expected a Map or Record")?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, k_ty, vec![]));
        self.live_collection_loop(ctx, "kv_keys", arr, |b, ctx, _, entry| {
            let (k, _) = b.entry_fields(ctx, entry)?;
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, k));
            Ok(())
        })?;
        Ok(out)
    }

    /// `values()` of a `Map` box (a live view) or a dictionary (a copied array).
    pub(super) fn kv_values(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        if map_box_record(ctx, arr.get_type(ctx)).is_some() {
            return self.alloc_collection_view(ctx, arr, CollectionViewKind::MapValues);
        }
        let (_, v_ty, _) =
            kv_entry_types(ctx, arr.get_type(ctx)).context("expected a Map or Record")?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, v_ty, vec![]));
        self.live_collection_loop(ctx, "kv_values", arr, |b, ctx, _, entry| {
            let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
            let v = b.append_value(ctx, &v_op);
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, v));
            Ok(())
        })?;
        Ok(out)
    }

    /// `entries()` of a `Map` box (a live view) or a dictionary (copied pairs).
    pub(super) fn kv_entries(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        if map_box_record(ctx, arr.get_type(ctx)).is_some() {
            return self.alloc_collection_view(ctx, arr, CollectionViewKind::MapEntries);
        }
        let (k_ty, v_ty, _) =
            kv_entry_types(ctx, arr.get_type(ctx)).context("expected a Map or Record")?;
        let tuple_rec = self.shapes.pair_type(ctx, k_ty, v_ty)?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        self.live_collection_loop(ctx, "kv_entries", arr, |b, ctx, _, entry| {
            let (k, v) = b.entry_fields(ctx, entry)?;
            let pair = b.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![k, v]));
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, pair));
            Ok(())
        })?;
        Ok(out)
    }
}
