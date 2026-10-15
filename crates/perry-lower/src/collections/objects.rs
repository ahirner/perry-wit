//! Key-value operations, `Record<K, V>` dictionaries, `Object.*`, `Array.isArray`, `in`, and `delete`.

use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(crate) fn collection_entry_live(
        &mut self,
        ctx: &mut Context,
        entry: Value,
    ) -> Result<Option<Value>> {
        let Ok(rec) = TypedHandle::<RecordType>::from_handle(entry.get_type(ctx), ctx) else {
            return Ok(None);
        };
        let field = {
            let rec_ref = rec.deref(ctx);
            let name = rec_ref.name();
            if name.as_ref().starts_with("map_tuple_") {
                "_2"
            } else if name.as_ref().starts_with("set_tuple_") {
                "_1"
            } else {
                return Ok(None);
            }
        };
        let op = FieldGetOp::new(ctx, entry, field.to_string())?;
        Ok(Some(self.append_value(ctx, &op)))
    }

    pub(crate) fn collection_size(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        let num_ty = FP64Type::get(ctx).into();
        let count_slot = self.entry_slot(ctx, num_ty);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        self.store(ctx, count_slot, zero);
        self.live_collection_loop(ctx, "coll_size", arr, |b, ctx, _, _| {
            let cur = b.load(ctx, count_slot)?;
            let next_c = b.binary(ctx, BinaryKind::Add, cur, one);
            b.store(ctx, count_slot, next_c);
            Ok(())
        })?;
        self.load(ctx, count_slot)
    }

    pub(crate) fn normalize_optional_read(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        let ty = val.get_type(ctx);
        if optional_value(ctx, ty).is_some()
            && absence(ctx, ty).is_some_and(|a| a.admits_undefined())
        {
            let is_undef = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, val));
            let undef = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty));
            return self.choose(ctx, ty, is_undef, |_, _| Ok(undef), |_, _| Ok(val));
        }
        Ok(val)
    }

    pub(crate) fn kv_get(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        key: Value,
        k_ty: TypeHandle,
        v_ty: TypeHandle,
    ) -> Result<Value> {
        let key = self.fit(ctx, key, k_ty)?;
        let states = crate::coerce::merge_absence(
            absence(ctx, v_ty),
            Some(perry_mir::attrs::Absence::Undefined),
        )
        .unwrap_or(perry_mir::attrs::Absence::Undefined);
        let inner = optional_value(ctx, v_ty).unwrap_or(v_ty);
        let opt_v_ty: TypeHandle = OptionalType::get_with(ctx, inner, states).into();
        let miss = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_v_ty));
        let result = self.entry_slot(ctx, opt_v_ty);
        self.store(ctx, result, miss);
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "kv_get", zero, limit, |b, ctx, idx, next, done| {
            let entry = b.element_at(ctx, arr, idx)?;
            if let Some(live) = b.collection_entry_live(ctx, entry)? {
                let live_bb = b.new_block(ctx, "kv_get_live");
                b.branch(ctx, live, live_bb, next);
                b.switch_to(live_bb);
            }
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let cand_k = b.append_value(ctx, &k_op);
            let matched = b.same_value_zero(ctx, cand_k, key, k_ty)?;
            let hit = b.new_block(ctx, "kv_get_hit");
            b.branch(ctx, matched, hit, next);
            b.switch_to(hit);
            let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
            let cand_v = b.append_value(ctx, &v_op);
            let wrapped = b.fit(ctx, cand_v, opt_v_ty)?;
            b.store(ctx, result, wrapped);
            b.jump(ctx, done);
            Ok(())
        })?;
        self.load(ctx, result)
    }

    /// Insert or overwrite `(key, val)` in a `Map<K, V>` or `Record<K, V>` backing array.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn kv_set(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        key: Value,
        val: Value,
        k_ty: TypeHandle,
        v_ty: TypeHandle,
        tuple_rec: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let key = self.fit(ctx, key, k_ty)?;
        let val = self.fit(ctx, val, v_ty)?;
        let is_map_tuple = tuple_rec
            .deref(ctx)
            .name()
            .as_ref()
            .starts_with("map_tuple_");
        let new_entry = if is_map_tuple {
            let live_true = self.boolean(ctx, true);
            self.emit_value(ctx, |ctx| {
                AllocOp::new(ctx, tuple_rec, vec![key, val, live_true])
            })
        } else {
            self.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![key, val]))
        };
        let bool_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let found_slot = self.entry_slot(ctx, bool_ty);
        let false_val = self.boolean(ctx, false);
        self.store(ctx, found_slot, false_val);
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "kv_set", zero, limit, |b, ctx, idx, next, done| {
            let entry = b.element_at(ctx, arr, idx)?;
            if let Some(live) = b.collection_entry_live(ctx, entry)? {
                let live_bb = b.new_block(ctx, "kv_set_live");
                b.branch(ctx, live, live_bb, next);
                b.switch_to(live_bb);
            }
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let cand_k = b.append_value(ctx, &k_op);
            let matched = b.same_value_zero(ctx, cand_k, key, k_ty)?;
            let hit = b.new_block(ctx, "kv_set_hit");
            b.branch(ctx, matched, hit, next);
            b.switch_to(hit);
            b.emit(ctx, |ctx| ArraySetOp::new(ctx, arr, idx, new_entry));
            let true_val = b.boolean(ctx, true);
            b.store(ctx, found_slot, true_val);
            b.jump(ctx, done);
            Ok(())
        })?;
        let was_found = self.load(ctx, found_slot)?;
        let append_bb = self.new_block(ctx, "kv_set_append");
        let after_bb = self.new_block(ctx, "kv_set_after");
        self.branch(ctx, was_found, after_bb, append_bb);
        self.switch_to(append_bb);
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, arr, new_entry));
        self.jump(ctx, after_bb);
        self.switch_to(after_bb);
        Ok(arr)
    }

    pub(crate) fn kv_has(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        key: Value,
        k_ty: TypeHandle,
    ) -> Result<Value> {
        let key = self.fit(ctx, key, k_ty)?;
        let bool_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let found_slot = self.entry_slot(ctx, bool_ty);
        let false_val = self.boolean(ctx, false);
        self.store(ctx, found_slot, false_val);
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "kv_has", zero, limit, |b, ctx, idx, next, done| {
            let entry = b.element_at(ctx, arr, idx)?;
            if let Some(live) = b.collection_entry_live(ctx, entry)? {
                let live_bb = b.new_block(ctx, "kv_has_live");
                b.branch(ctx, live, live_bb, next);
                b.switch_to(live_bb);
            }
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let cand_k = b.append_value(ctx, &k_op);
            let matched = b.same_value_zero(ctx, cand_k, key, k_ty)?;
            let hit = b.new_block(ctx, "kv_has_hit");
            b.branch(ctx, matched, hit, next);
            b.switch_to(hit);
            let true_val = b.boolean(ctx, true);
            b.store(ctx, found_slot, true_val);
            b.jump(ctx, done);
            Ok(())
        })?;
        self.load(ctx, found_slot)
    }

    pub(crate) fn kv_delete(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        key: Value,
        k_ty: TypeHandle,
    ) -> Result<Value> {
        let key = self.fit(ctx, key, k_ty)?;
        let bool_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let found_slot = self.entry_slot(ctx, bool_ty);
        let false_val = self.boolean(ctx, false);
        self.store(ctx, found_slot, false_val);
        if map_entry_types(ctx, arr.get_type(ctx)).is_some() {
            let limit = self.array_length(ctx, arr);
            let zero = self.number(ctx, 0.0);
            self.index_loop(ctx, "map_delete", zero, limit, |b, ctx, idx, next, done| {
                let entry = b.element_at(ctx, arr, idx)?;
                if let Some(live) = b.collection_entry_live(ctx, entry)? {
                    let live_bb = b.new_block(ctx, "map_delete_live");
                    b.branch(ctx, live, live_bb, next);
                    b.switch_to(live_bb);
                }
                let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
                let cand_k = b.append_value(ctx, &k_op);
                let matched = b.same_value_zero(ctx, cand_k, key, k_ty)?;
                let hit = b.new_block(ctx, "map_delete_hit");
                b.branch(ctx, matched, hit, next);
                b.switch_to(hit);
                let dead = b.boolean(ctx, false);
                b.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, entry, "_2".to_string(), dead)
                });
                let true_val = b.boolean(ctx, true);
                b.store(ctx, found_slot, true_val);
                b.jump(ctx, done);
                Ok(())
            })?;
            return self.load(ctx, found_slot);
        }
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

    pub(super) fn collection_clear(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        let is_map = map_entry_types(ctx, arr.get_type(ctx)).is_some();
        let live_field = if is_map { "_2" } else { "_1" };
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        let false_val = self.boolean(ctx, false);
        self.index_loop(ctx, "coll_clear", zero, limit, |b, ctx, idx, _, _| {
            let entry = b.element_at(ctx, arr, idx)?;
            b.emit(ctx, |ctx| {
                FieldSetOp::new(ctx, entry, live_field.to_string(), false_val)
            });
            Ok(())
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    pub(super) fn live_collection_loop(
        &mut self,
        ctx: &mut Context,
        label: &str,
        arr: Value,
        body: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<()>,
    ) -> Result<()> {
        let zero = self.number(ctx, 0.0);
        let index = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, index, zero);
        let header = self.new_block(ctx, &format!("{label}_header"));
        let check = self.new_block(ctx, &format!("{label}_check"));
        let entered = self.new_block(ctx, &format!("{label}_body"));
        let next = self.new_block(ctx, &format!("{label}_next"));
        let done = self.new_block(ctx, &format!("{label}_done"));
        self.jump(ctx, header);
        self.switch_to(header);
        let current = self.load(ctx, index)?;
        let limit = self.array_length(ctx, arr);
        let more = self.compare(ctx, CompareKind::Lt, current, limit);
        self.branch(ctx, more, check, done);
        self.switch_to(check);
        let entry = self.element_at(ctx, arr, current)?;
        if let Some(live) = self.collection_entry_live(ctx, entry)? {
            self.branch(ctx, live, entered, next);
            self.switch_to(entered);
        } else {
            self.jump(ctx, entered);
            self.switch_to(entered);
        }
        body(self, ctx, current, entry)?;
        self.jump(ctx, next);
        self.switch_to(next);
        let one = self.number(ctx, 1.0);
        let advanced = self.binary(ctx, BinaryKind::Add, current, one);
        self.store(ctx, index, advanced);
        self.jump(ctx, header);
        self.switch_to(done);
        Ok(())
    }

    pub(super) fn map_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        self.live_collection_loop(ctx, "map_foreach", arr, |b, ctx, _, entry| {
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let k = b.append_value(ctx, &k_op);
            let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
            let v = b.append_value(ctx, &v_op);
            b.array_callback(ctx, callback, &[v, k, arr])?;
            Ok(())
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    pub(super) fn set_add(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        val: Value,
        elem_ty: TypeHandle,
        tuple_rec: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let val = self.fit(ctx, val, elem_ty)?;
        let present = self.set_has(ctx, arr, val, elem_ty)?;
        let append_bb = self.new_block(ctx, "set_add_append");
        let after_bb = self.new_block(ctx, "set_add_after");
        self.branch(ctx, present, after_bb, append_bb);
        self.switch_to(append_bb);
        let live_true = self.boolean(ctx, true);
        let entry = self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, tuple_rec, vec![val, live_true])
        });
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, arr, entry));
        self.jump(ctx, after_bb);
        self.switch_to(after_bb);
        Ok(arr)
    }

    pub(super) fn set_has(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        val: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let val = self.fit(ctx, val, elem_ty)?;
        let bool_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let found_slot = self.entry_slot(ctx, bool_ty);
        let false_val = self.boolean(ctx, false);
        self.store(ctx, found_slot, false_val);
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "set_has", zero, limit, |b, ctx, idx, next, done| {
            let entry = b.element_at(ctx, arr, idx)?;
            if let Some(live) = b.collection_entry_live(ctx, entry)? {
                let live_bb = b.new_block(ctx, "set_has_live");
                b.branch(ctx, live, live_bb, next);
                b.switch_to(live_bb);
            }
            let op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let cand = b.append_value(ctx, &op);
            let matched = b.same_value_zero(ctx, cand, val, elem_ty)?;
            let hit = b.new_block(ctx, "set_has_hit");
            b.branch(ctx, matched, hit, next);
            b.switch_to(hit);
            let true_val = b.boolean(ctx, true);
            b.store(ctx, found_slot, true_val);
            b.jump(ctx, done);
            Ok(())
        })?;
        self.load(ctx, found_slot)
    }

    pub(super) fn set_delete(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        val: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let val = self.fit(ctx, val, elem_ty)?;
        let bool_ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let found_slot = self.entry_slot(ctx, bool_ty);
        let false_val = self.boolean(ctx, false);
        self.store(ctx, found_slot, false_val);
        let zero = self.number(ctx, 0.0);
        let limit = self.array_length(ctx, arr);
        self.index_loop(ctx, "set_delete", zero, limit, |b, ctx, idx, next, done| {
            let entry = b.element_at(ctx, arr, idx)?;
            if let Some(live) = b.collection_entry_live(ctx, entry)? {
                let live_bb = b.new_block(ctx, "set_delete_live");
                b.branch(ctx, live, live_bb, next);
                b.switch_to(live_bb);
            }
            let v_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let cand = b.append_value(ctx, &v_op);
            let matched = b.same_value_zero(ctx, cand, val, elem_ty)?;
            let hit = b.new_block(ctx, "set_delete_hit");
            b.branch(ctx, matched, hit, next);
            b.switch_to(hit);
            let dead = b.boolean(ctx, false);
            b.emit(ctx, |ctx| {
                FieldSetOp::new(ctx, entry, "_1".to_string(), dead)
            });
            let true_val = b.boolean(ctx, true);
            b.store(ctx, found_slot, true_val);
            b.jump(ctx, done);
            Ok(())
        })?;
        self.load(ctx, found_slot)
    }

    pub(crate) fn set_values(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![]));
        self.live_collection_loop(ctx, "set_values", arr, |b, ctx, _, entry| {
            let op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let item = b.append_value(ctx, &op);
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, item));
            Ok(())
        })?;
        Ok(out)
    }

    pub(super) fn set_entries(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let tuple_rec = self.shapes.mir_record_prefix(
            ctx,
            vec![("_0".to_string(), elem_ty), ("_1".to_string(), elem_ty)],
            Some("tuple"),
        )?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        self.live_collection_loop(ctx, "set_entries", arr, |b, ctx, _, entry| {
            let op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let item = b.append_value(ctx, &op);
            let pair = b.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![item, item]));
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, pair));
            Ok(())
        })?;
        Ok(out)
    }

    pub(super) fn set_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        self.live_collection_loop(ctx, "set_foreach", arr, |b, ctx, _, entry| {
            let op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let item = b.append_value(ctx, &op);
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
        let tuple_rec = self.shapes.mir_record_prefix(
            ctx,
            vec![("_0".to_string(), num_ty), ("_1".to_string(), elem_ty)],
            Some("tuple"),
        )?;
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

    pub(super) fn array_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        let limit = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "foreach", zero, limit, |b, ctx, idx, _, _| {
            let item = b.element_at(ctx, arr, idx)?;
            b.array_callback(ctx, callback, &[item, idx, arr])?;
            Ok(())
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    pub(crate) fn kv_keys(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        let (k_ty, _, _) =
            kv_entry_types(ctx, arr.get_type(ctx)).context("expected a Map or Record")?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, k_ty, vec![]));
        self.live_collection_loop(ctx, "kv_keys", arr, |b, ctx, _, entry| {
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let k = b.append_value(ctx, &k_op);
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, k));
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn kv_values(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
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

    pub(crate) fn kv_entries(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        let (k_ty, v_ty, _) =
            kv_entry_types(ctx, arr.get_type(ctx)).context("expected a Map or Record")?;
        let tuple_rec = self.shapes.mir_record_prefix(
            ctx,
            vec![("_0".to_string(), k_ty), ("_1".to_string(), v_ty)],
            Some("tuple"),
        )?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        self.live_collection_loop(ctx, "kv_entries", arr, |b, ctx, _, entry| {
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let k = b.append_value(ctx, &k_op);
            let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
            let v = b.append_value(ctx, &v_op);
            let pair = b.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![k, v]));
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, pair));
            Ok(())
        })?;
        Ok(out)
    }

    /// Lower an object literal `{ k1: v1, ... }` when expected type is `Record<K, V>`.
    pub(crate) fn dict_literal(
        &mut self,
        ctx: &mut Context,
        names: &[&str],
        args: &'s [Expr],
        k_ty: TypeHandle,
        v_ty: TypeHandle,
        tuple_rec: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let dict = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        for (name, val_expr) in names.iter().zip(args) {
            let k = self.string(ctx, name);
            let v = self.expression_with(ctx, val_expr, Some(v_ty))?;
            self.kv_set(ctx, dict, k, v, k_ty, v_ty, tuple_rec)?;
        }
        Ok(dict)
    }

    /// Dynamic string indexing `rec[key]` on a closed `RecordType` when all fields share a uniform type.
    pub(crate) fn record_dynamic_get(
        &mut self,
        ctx: &mut Context,
        rec: Value,
        key: Value,
        rec_handle: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let (fields, uniform_ty) = record_uniform_field_type(ctx, rec_handle).context(
            "dynamic indexing on a closed record requires all fields to share the same type, or use Record<string, V>",
        )?;
        let states = crate::coerce::merge_absence(
            absence(ctx, uniform_ty),
            Some(perry_mir::attrs::Absence::Undefined),
        )
        .unwrap_or(perry_mir::attrs::Absence::Undefined);
        let inner = optional_value(ctx, uniform_ty).unwrap_or(uniform_ty);
        let opt_ty: TypeHandle = OptionalType::get_with(ctx, inner, states).into();
        let mut acc = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty));
        for (name, _) in fields.into_iter().rev() {
            let name_str = self.string(ctx, &name);
            let eq = self.compare(ctx, CompareKind::Eq, key, name_str);
            let op = FieldGetOp::new(ctx, rec, name)?;
            let field_val = self.append_value(ctx, &op);
            let normalized = self.normalize_optional_read(ctx, field_val)?;
            let field_opt = self.fit(ctx, normalized, opt_ty)?;
            acc = self.choose(ctx, opt_ty, eq, |_, _| Ok(field_opt), |_, _| Ok(acc))?;
        }
        Ok(acc)
    }

    /// `Object.keys(obj)` and `for (const k in obj)`.
    pub(crate) fn object_keys(&mut self, ctx: &mut Context, obj_expr: &'s Expr) -> Result<Value> {
        let obj = self.expression(ctx, obj_expr)?;
        let obj = self.defined(ctx, obj)?;
        let ty = obj.get_type(ctx);
        if dict_entry_types(ctx, ty).is_some() {
            return self.kv_keys(ctx, obj);
        }
        if let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            let fields = rec
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .to_vec();
            let str_ty: TypeHandle = StringType::get(ctx).into();
            let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, str_ty, vec![]));
            for (name, field_ty) in fields {
                let name_val = self.string(ctx, &name);
                if optional_value(ctx, field_ty).is_some()
                    && absence(ctx, field_ty).is_some_and(|a| a.admits_undefined())
                {
                    let op = FieldPresentOp::new(ctx, obj, name)?;
                    let present = self.append_value(ctx, &op);
                    let keep_bb = self.new_block(ctx, "obj_key_keep");
                    let after_bb = self.new_block(ctx, "obj_key_after");
                    self.branch(ctx, present, keep_bb, after_bb);
                    self.switch_to(keep_bb);
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, name_val));
                    self.jump(ctx, after_bb);
                    self.switch_to(after_bb);
                } else {
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, name_val));
                }
            }
            return Ok(out);
        }
        if let Some(json) = self.json_operand(ctx, obj)? {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonKeys, vec![json]));
        }
        bail!("Object.keys requires a Record, interface/object shape, or JSON value")
    }

    /// `Object.values(obj)`.
    pub(crate) fn object_values(&mut self, ctx: &mut Context, obj_expr: &'s Expr) -> Result<Value> {
        let obj = self.expression(ctx, obj_expr)?;
        let obj = self.defined(ctx, obj)?;
        let ty = obj.get_type(ctx);
        if dict_entry_types(ctx, ty).is_some() {
            return self.kv_values(ctx, obj);
        }
        if let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            let (fields, uniform_ty) = record_uniform_field_type(ctx, rec).context(
                "Object.values on a closed record requires all fields to share a uniform value type",
            )?;
            let elem_ty = uniform_ty;
            let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![]));
            for (name, field_ty) in fields {
                let op = FieldGetOp::new(ctx, obj, name.clone())?;
                let val = self.append_value(ctx, &op);
                if optional_value(ctx, field_ty).is_some()
                    && absence(ctx, field_ty).is_some_and(|a| a.admits_undefined())
                {
                    let pop = FieldPresentOp::new(ctx, obj, name)?;
                    let present = self.append_value(ctx, &pop);
                    let keep_bb = self.new_block(ctx, "obj_val_keep");
                    let after_bb = self.new_block(ctx, "obj_val_after");
                    self.branch(ctx, present, keep_bb, after_bb);
                    self.switch_to(keep_bb);
                    let normalized = self.normalize_optional_read(ctx, val)?;
                    let fitted = self.fit(ctx, normalized, elem_ty)?;
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, fitted));
                    self.jump(ctx, after_bb);
                    self.switch_to(after_bb);
                } else {
                    let fitted = self.fit(ctx, val, elem_ty)?;
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, fitted));
                }
            }
            return Ok(out);
        }
        bail!("Object.values requires a Record<K, V> or uniform record shape")
    }

    /// `Object.entries(obj)`.
    pub(crate) fn object_entries(
        &mut self,
        ctx: &mut Context,
        obj_expr: &'s Expr,
    ) -> Result<Value> {
        let obj = self.expression(ctx, obj_expr)?;
        let obj = self.defined(ctx, obj)?;
        let ty = obj.get_type(ctx);
        if dict_entry_types(ctx, ty).is_some() {
            return self.kv_entries(ctx, obj);
        }
        if let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            let (fields, uniform_ty) = record_uniform_field_type(ctx, rec).context(
                "Object.entries on a closed record requires all fields to share a uniform value type",
            )?;
            let str_ty: TypeHandle = StringType::get(ctx).into();
            let elem_ty = uniform_ty;
            let tuple_rec = self.shapes.mir_record_prefix(
                ctx,
                vec![("_0".to_string(), str_ty), ("_1".to_string(), elem_ty)],
                Some("tuple"),
            )?;
            let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
            for (name, field_ty) in fields {
                let name_val = self.string(ctx, &name);
                let op = FieldGetOp::new(ctx, obj, name.clone())?;
                let val = self.append_value(ctx, &op);
                if optional_value(ctx, field_ty).is_some()
                    && absence(ctx, field_ty).is_some_and(|a| a.admits_undefined())
                {
                    let pop = FieldPresentOp::new(ctx, obj, name)?;
                    let present = self.append_value(ctx, &pop);
                    let keep_bb = self.new_block(ctx, "obj_ent_keep");
                    let after_bb = self.new_block(ctx, "obj_ent_after");
                    self.branch(ctx, present, keep_bb, after_bb);
                    self.switch_to(keep_bb);
                    let normalized = self.normalize_optional_read(ctx, val)?;
                    let fitted = self.fit(ctx, normalized, elem_ty)?;
                    let pair = self.emit_value(ctx, |ctx| {
                        AllocOp::new(ctx, tuple_rec, vec![name_val, fitted])
                    });
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, pair));
                    self.jump(ctx, after_bb);
                    self.switch_to(after_bb);
                } else {
                    let fitted = self.fit(ctx, val, elem_ty)?;
                    let pair = self.emit_value(ctx, |ctx| {
                        AllocOp::new(ctx, tuple_rec, vec![name_val, fitted])
                    });
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, pair));
                }
            }
            return Ok(out);
        }
        bail!("Object.entries requires a Record<K, V> or uniform record shape")
    }

    /// `Object.fromEntries(entries)`.
    pub(crate) fn object_from_entries(
        &mut self,
        ctx: &mut Context,
        entries_expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let entries_val = self.expression_with(ctx, entries_expr, expected)?;
        let entries_val = self.defined(ctx, entries_val)?;
        let (k_ty, v_ty, _) = kv_entry_types(ctx, entries_val.get_type(ctx))
            .or_else(|| expected.and_then(|e| kv_entry_types(ctx, e)))
            .context("Object.fromEntries requires an iterable of [key, value] pairs")?;
        let dict_tuple = match expected.and_then(|e| dict_entry_types(ctx, e).map(|(_, _, r)| r)) {
            Some(r) => r,
            None => self.shapes.mir_record_prefix(
                ctx,
                vec![("_0".to_string(), k_ty), ("_1".to_string(), v_ty)],
                Some("dict_tuple"),
            )?,
        };
        let dict = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, dict_tuple.into(), vec![]));
        self.live_collection_loop(ctx, "from_entries", entries_val, |b, ctx, _, entry| {
            let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
            let k = b.append_value(ctx, &k_op);
            let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
            let v = b.append_value(ctx, &v_op);
            b.kv_set(ctx, dict, k, v, k_ty, v_ty, dict_tuple)?;
            Ok(())
        })?;
        Ok(dict)
    }

    /// `Object.is(a, b)` — SameValue equality (`NaN` equals `NaN`, `+0` does not equal `-0`).
    pub(crate) fn object_is(
        &mut self,
        ctx: &mut Context,
        left: &'s Expr,
        right: &'s Expr,
    ) -> Result<Value> {
        let lhs = self.expression(ctx, left)?;
        let rhs = self.expression(ctx, right)?;
        if lhs.get_type(ctx).deref(ctx).is::<FP64Type>()
            && rhs.get_type(ctx).deref(ctx).is::<FP64Type>()
        {
            let eq = self.compare(ctx, CompareKind::Eq, lhs, rhs);
            let bool_ty = eq.get_type(ctx);
            let a_nan = self.compare(ctx, CompareKind::Ne, lhs, lhs);
            let b_nan = self.compare(ctx, CompareKind::Ne, rhs, rhs);
            let both_nan = self.choose(ctx, bool_ty, a_nan, |_, _| Ok(b_nan), |_, _| Ok(a_nan))?;
            let zero = self.number(ctx, 0.0);
            let is_zero = self.compare(ctx, CompareKind::Eq, lhs, zero);
            let one = self.number(ctx, 1.0);
            let inv_a = self.binary(ctx, BinaryKind::Div, one, lhs);
            let inv_b = self.binary(ctx, BinaryKind::Div, one, rhs);
            let same_sign_zero = self.compare(ctx, CompareKind::Eq, inv_a, inv_b);
            let eq_non_neg_zero = self.choose(
                ctx,
                bool_ty,
                is_zero,
                |_, _| Ok(same_sign_zero),
                |b, ctx| Ok(b.boolean(ctx, true)),
            )?;
            return self.choose(
                ctx,
                bool_ty,
                eq,
                |_, _| Ok(eq_non_neg_zero),
                |_, _| Ok(both_nan),
            );
        }
        self.comparison(ctx, &CompareOp::Eq, left, right)
    }

    /// `Array.isArray(val)`.
    pub(crate) fn array_is_array(
        &mut self,
        ctx: &mut Context,
        val_expr: &'s Expr,
    ) -> Result<Value> {
        let val = self.expression(ctx, val_expr)?;
        let ty = val.get_type(ctx);
        let inner = optional_value(ctx, ty).unwrap_or(ty);
        let is_plain_array = inner.deref(ctx).is::<ArrayType>()
            && map_entry_types(ctx, inner).is_none()
            && dict_entry_types(ctx, inner).is_none()
            && set_entry_type(ctx, inner).is_none();
        if ty.deref(ctx).is::<OptionalType>() {
            if !is_plain_array {
                return Ok(self.boolean(ctx, false));
            }
            let nullish = self.is_nullish(ctx, val)?;
            let false_val = self.boolean(ctx, false);
            return Ok(self.compare(ctx, CompareKind::Eq, nullish, false_val));
        }
        Ok(self.boolean(ctx, is_plain_array))
    }

    /// `key in obj`.
    pub(crate) fn in_operator(
        &mut self,
        ctx: &mut Context,
        prop_expr: &'s Expr,
        obj_expr: &'s Expr,
    ) -> Result<Value> {
        let obj = self.expression(ctx, obj_expr)?;
        let obj = self.defined(ctx, obj)?;
        let ty = obj.get_type(ctx);
        if let Some((k_ty, _, _)) = dict_entry_types(ctx, ty) {
            let k = self.expression_with(ctx, prop_expr, Some(k_ty))?;
            return self.kv_has(ctx, obj, k, k_ty);
        }
        if let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            let k = self.string_operand(ctx, prop_expr)?;
            let fields = rec
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .to_vec();
            let mut acc = self.boolean(ctx, false);
            let bool_ty = acc.get_type(ctx);
            for (name, field_ty) in fields {
                let name_val = self.string(ctx, &name);
                let eq = self.compare(ctx, CompareKind::Eq, k, name_val);
                acc = self.choose(
                    ctx,
                    bool_ty,
                    eq,
                    |b, ctx| {
                        if optional_value(ctx, field_ty).is_some()
                            && absence(ctx, field_ty).is_some_and(|a| a.admits_undefined())
                        {
                            let op = FieldPresentOp::new(ctx, obj, name)?;
                            Ok(b.append_value(ctx, &op))
                        } else {
                            Ok(b.boolean(ctx, true))
                        }
                    },
                    |_, _| Ok(acc),
                )?;
            }
            return Ok(acc);
        }
        bail!("'in' operator requires a Record<K, V> or closed record shape")
    }

    /// `delete obj[key]` or `delete obj.prop` on a `Record<K, V>` dictionary or optional record field.
    pub(crate) fn delete_operator(&mut self, ctx: &mut Context, inner: &'s Expr) -> Result<Value> {
        match inner {
            Expr::IndexGet { object, index } => {
                let obj = self.expression(ctx, object)?;
                let obj = self.defined(ctx, obj)?;
                if let Some((k_ty, _, _)) = dict_entry_types(ctx, obj.get_type(ctx)) {
                    let k = self.expression_with(ctx, index, Some(k_ty))?;
                    self.kv_delete(ctx, obj, k, k_ty)?;
                    return Ok(self.boolean(ctx, true));
                }
                bail!("delete operator is only supported on Record<K, V> dictionary types")
            }
            Expr::PropertyGet {
                object, property, ..
            } => {
                let obj = self.expression(ctx, object)?;
                let obj = self.defined(ctx, obj)?;
                let ty = obj.get_type(ctx);
                if let Some((k_ty, _, _)) = dict_entry_types(ctx, ty) {
                    let k = self.string(ctx, property);
                    self.kv_delete(ctx, obj, k, k_ty)?;
                    return Ok(self.boolean(ctx, true));
                }
                let opt_field = TypedHandle::<RecordType>::from_handle(ty, ctx)
                    .ok()
                    .and_then(|rec| rec.deref(ctx).field(property).map(|(_, t)| t));
                if let Some(field_ty) = opt_field
                    && optional_value(ctx, field_ty).is_some()
                    && absence(ctx, field_ty).is_some_and(|a| a.admits_undefined())
                {
                    let omitted = self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, field_ty));
                    self.emit(ctx, |ctx| {
                        FieldSetOp::new(ctx, obj, property.clone(), omitted)
                    });
                    return Ok(self.boolean(ctx, true));
                }
                bail!(
                    "delete operator is only supported on Record<K, V> dictionary types or optional record fields"
                )
            }
            _ => bail!("delete operator requires a property or index access on a Record<K, V>"),
        }
    }
}
