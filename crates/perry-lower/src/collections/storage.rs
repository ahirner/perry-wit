//! Live collection storage: boxes, views, cursors, tombstones, compaction and live iteration.

use super::*;

/// A cursor registered in a collection's cursor array, so deletes and compaction move it.
#[derive(Clone, Copy)]
pub(crate) enum LiveCursor {
    /// A view's own cursor: it survives `break`, and exhaustion parks it at infinity.
    View { cursors: Value, cursor: Value },
    /// A cursor registered for one loop and released on every exit.
    Loop { cursors: Value, cursor: Value },
}

impl LiveCursor {
    /// Start iterating `coll` at the index stored in `index`, or at a view's own position,
    /// which is stored into `index`. `None` when `coll` has no cursor array (plain arrays).
    pub(crate) fn open(
        b: &mut FunctionBuilder<'_>,
        ctx: &mut Context,
        coll: Value,
        index: Value,
    ) -> Result<Option<Self>> {
        let Some(cursors) = b.collection_cursors_array(ctx, coll)? else {
            return Ok(None);
        };
        Ok(Some(
            if collection_view_record(ctx, coll.get_type(ctx)).is_some() {
                let op = FieldGetOp::new(ctx, coll, "_1".to_string())?;
                let cursor = b.append_value(ctx, &op);
                let start = Self::View { cursors, cursor }.position(b, ctx)?;
                b.store(ctx, index, start);
                Self::View { cursors, cursor }
            } else {
                let start = b.load(ctx, index)?;
                let cursor = b.push_collection_cursor(ctx, cursors, start)?;
                Self::Loop { cursors, cursor }
            },
        ))
    }

    /// Record that the next visit starts at `next`, before the body can delete entries.
    pub(crate) fn advance(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context, next: Value) {
        let (Self::View { cursor, .. } | Self::Loop { cursor, .. }) = self;
        b.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, cursor, "_0".to_string(), next)
        });
    }

    /// The index the next visit starts at, as moved by deletes and compaction.
    pub(crate) fn position(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context) -> Result<Value> {
        let (Self::View { cursor, .. } | Self::Loop { cursor, .. }) = self;
        let op = FieldGetOp::new(ctx, cursor, "_0".to_string())?;
        Ok(b.append_value(ctx, &op))
    }

    /// The loop ran out of entries: a view stays exhausted and leaves the cursor array.
    pub(crate) fn exhausted(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context) -> Result<()> {
        match self {
            Self::View { cursors, cursor } => {
                let inf = b.number(ctx, f64::INFINITY);
                b.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, cursor, "_0".to_string(), inf)
                });
                b.remove_collection_cursor(ctx, cursors, cursor)
            }
            Self::Loop { .. } => Ok(()),
        }
    }

    /// Control left the loop by any exit: a loop's own cursor is released.
    pub(crate) fn exited(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context) -> Result<()> {
        match self {
            Self::Loop { cursors, cursor } => b.remove_collection_cursor(ctx, cursors, cursor),
            Self::View { .. } => Ok(()),
        }
    }
}

impl<'s> FunctionBuilder<'s> {
    /// A new, empty `kind` box over `entry` records.
    pub(super) fn alloc_collection_box(
        &mut self,
        ctx: &mut Context,
        entry: TypedHandle<RecordType>,
        kind: EntryKind,
    ) -> Result<Value> {
        let box_rec = self.shapes.collection_box_type(ctx, entry, kind)?;
        let cursor_rec = self.shapes.cursor_tuple_type(ctx)?;
        let entries = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, entry.into(), vec![]));
        let zero = self.number(ctx, 0.0);
        let cursors = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, cursor_rec.into(), vec![]));
        Ok(self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, box_rec, vec![entries, zero, cursors])
        }))
    }

    /// A `view` over `box_val` with its own cursor at the first entry.
    pub(super) fn alloc_collection_view(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        view: CollectionViewKind,
    ) -> Result<Value> {
        let cursors_op = FieldGetOp::new(ctx, box_val, "_2".to_string())?;
        let cursors = self.append_value(ctx, &cursors_op);
        let zero = self.number(ctx, 0.0);
        let cursor = self.push_collection_cursor(ctx, cursors, zero)?;
        let view_rec = self
            .shapes
            .collection_view_type(ctx, box_val.get_type(ctx), view)?;
        Ok(self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, view_rec, vec![box_val, cursor])
        }))
    }

    /// The box of a `Map`/`Set` or of a view; `None` for plain arrays.
    pub(super) fn collection_underlying_box(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Option<Value>> {
        let ty = val.get_type(ctx);
        if map_box_record(ctx, ty).is_some() || set_box_record(ctx, ty).is_some() {
            Ok(Some(val))
        } else if collection_view_record(ctx, ty).is_some() {
            let op = FieldGetOp::new(ctx, val, "_0".to_string())?;
            Ok(Some(self.append_value(ctx, &op)))
        } else {
            Ok(None)
        }
    }

    /// The entries array of a collection or view; plain arrays are their own entries.
    pub(crate) fn collection_entries_array(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        match self.collection_underlying_box(ctx, val)? {
            Some(box_val) => {
                let op = FieldGetOp::new(ctx, box_val, "_0".to_string())?;
                Ok(self.append_value(ctx, &op))
            }
            None => Ok(val),
        }
    }

    fn collection_cursors_array(&mut self, ctx: &mut Context, val: Value) -> Result<Option<Value>> {
        match self.collection_underlying_box(ctx, val)? {
            Some(box_val) => {
                let op = FieldGetOp::new(ctx, box_val, "_2".to_string())?;
                Ok(Some(self.append_value(ctx, &op)))
            }
            None => Ok(None),
        }
    }

    /// The number of entry slots, tombstones included: the bound of a raw index loop.
    pub(crate) fn collection_backing_length(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        let entries = self.collection_entries_array(ctx, val)?;
        Ok(self.array_length(ctx, entries))
    }

    /// The value iteration yields for entry slot `idx`: a key, value, element or pair.
    pub(crate) fn collection_view_element_at(
        &mut self,
        ctx: &mut Context,
        val: Value,
        idx: Value,
    ) -> Result<Value> {
        let ty = optional_value(ctx, val.get_type(ctx)).unwrap_or(val.get_type(ctx));
        let entries = self.collection_entries_array(ctx, val)?;
        let entry = self.element_at(ctx, entries, idx)?;
        let yielded = match collection_view_record(ctx, ty) {
            Some((view, _)) => Some(view),
            None if set_box_record(ctx, ty).is_some() => Some(CollectionViewKind::SetValues),
            None if map_box_record(ctx, ty).is_some() => Some(CollectionViewKind::MapEntries),
            None => None,
        };
        match yielded {
            Some(CollectionViewKind::MapKeys | CollectionViewKind::SetValues) => {
                let op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
                Ok(self.append_value(ctx, &op))
            }
            Some(CollectionViewKind::MapValues) => {
                let op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
                Ok(self.append_value(ctx, &op))
            }
            Some(CollectionViewKind::MapEntries) => {
                let (k, v) = self.entry_fields(ctx, entry)?;
                self.pair(ctx, k, v)
            }
            Some(CollectionViewKind::SetEntries) => {
                let op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
                let item = self.append_value(ctx, &op);
                self.pair(ctx, item, item)
            }
            None => Ok(entry),
        }
    }

    /// The key (`_0`) and value (`_1`) of a key-value entry or pair.
    pub(crate) fn entry_fields(
        &mut self,
        ctx: &mut Context,
        entry: Value,
    ) -> Result<(Value, Value)> {
        let k_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
        let k = self.append_value(ctx, &k_op);
        let v_op = FieldGetOp::new(ctx, entry, "_1".to_string())?;
        let v = self.append_value(ctx, &v_op);
        Ok((k, v))
    }

    /// A fresh `[first, second]` tuple.
    fn pair(&mut self, ctx: &mut Context, first: Value, second: Value) -> Result<Value> {
        let (first_ty, second_ty) = (first.get_type(ctx), second.get_type(ctx));
        let tuple_rec = self.shapes.pair_type(ctx, first_ty, second_ty)?;
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![first, second])))
    }

    /// The element type iterating `ty` yields: an array element, set value, view item or pair.
    pub(super) fn iterable_element_type(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
    ) -> Result<TypeHandle> {
        let ty = optional_value(ctx, ty).unwrap_or(ty);
        if let Some((elem_ty, _)) = set_entry_type(ctx, ty) {
            return Ok(elem_ty);
        }
        if let Some((k_ty, v_ty, _)) = map_entry_types(ctx, ty) {
            return Ok(self.shapes.pair_type(ctx, k_ty, v_ty)?.into());
        }
        if let Some((view, view_rec)) = collection_view_record(ctx, ty) {
            let (_, inner_box) = view_rec
                .deref(ctx)
                .field("_0")
                .context("collection view missing _0 field")?;
            let expected = || format!("expected a {} box", view.type_name());
            return match view {
                CollectionViewKind::MapKeys
                | CollectionViewKind::MapValues
                | CollectionViewKind::MapEntries => {
                    let (k_ty, v_ty, _) = map_entry_types(ctx, inner_box).with_context(expected)?;
                    Ok(match view {
                        CollectionViewKind::MapKeys => k_ty,
                        CollectionViewKind::MapValues => v_ty,
                        _ => self.shapes.pair_type(ctx, k_ty, v_ty)?.into(),
                    })
                }
                CollectionViewKind::SetValues | CollectionViewKind::SetEntries => {
                    let (elem_ty, _) = set_entry_type(ctx, inner_box).with_context(expected)?;
                    Ok(if view == CollectionViewKind::SetEntries {
                        self.shapes.pair_type(ctx, elem_ty, elem_ty)?.into()
                    } else {
                        elem_ty
                    })
                }
            };
        }
        if let Ok(arr) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            return Ok(arr.deref(ctx).element());
        }
        bail!("expected an array or collection iterable")
    }

    /// Copy what iterating `coll` yields into a fresh array.
    pub(crate) fn materialize_collection_to_array(
        &mut self,
        ctx: &mut Context,
        coll: Value,
    ) -> Result<Value> {
        let elem_ty = self.iterable_element_type(ctx, coll.get_type(ctx))?;
        let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![]));
        self.live_collection_loop(ctx, "materialize_iter", coll, |b, ctx, _, item| {
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, item));
            Ok(())
        })?;
        Ok(out)
    }

    fn push_collection_cursor(
        &mut self,
        ctx: &mut Context,
        cursors: Value,
        idx: Value,
    ) -> Result<Value> {
        let cursor_rec = self.shapes.cursor_tuple_type(ctx)?;
        let cursor = self.emit_value(ctx, |ctx| AllocOp::new(ctx, cursor_rec, vec![idx]));
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, cursors, cursor));
        Ok(cursor)
    }

    /// Swap-remove `target` from `cursors`; cursor order carries no meaning.
    fn remove_collection_cursor(
        &mut self,
        ctx: &mut Context,
        cursors: Value,
        target: Value,
    ) -> Result<()> {
        let len = self.array_length(ctx, cursors);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
        self.index_loop(ctx, "remove_cursor", zero, len, |b, ctx, ci, next, done| {
            let c = b.element_at(ctx, cursors, ci)?;
            let matched = b.compare(ctx, CompareKind::Eq, c, target);
            let hit = b.new_block(ctx, "remove_cursor_hit");
            b.branch(ctx, matched, hit, next);
            b.switch_to(hit);
            let last_idx = b.binary(ctx, BinaryKind::Sub, len, one);
            let last_elem = b.element_at(ctx, cursors, last_idx)?;
            b.emit(ctx, |ctx| ArraySetOp::new(ctx, cursors, ci, last_elem));
            let new_len = b.emit_value(ctx, |ctx| NumericCastOp::new(ctx, last_idx, u32_ty));
            b.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, cursors, new_len));
            b.jump(ctx, done);
            Ok(())
        })
    }

    /// Mark `entry` deleted, overwrite its `cleared` fields so they hold no references, and
    /// compact the owning box, if any.
    pub(super) fn tombstone_entry(
        &mut self,
        ctx: &mut Context,
        entry: Value,
        kind: EntryKind,
        cleared: &[(&str, TypeHandle)],
        owner: Option<Value>,
    ) -> Result<()> {
        let dead = self.boolean(ctx, false);
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, entry, kind.live_field().to_string(), dead)
        });
        for &(field, ty) in cleared {
            if let Some(dummy) = self.zero_value_for_type(ctx, ty) {
                self.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, entry, field.to_string(), dummy)
                });
            }
        }
        if let Some(box_val) = owner {
            self.bump_live_count(ctx, box_val, BinaryKind::Sub)?;
            self.reclaim_after_delete(ctx, box_val, kind)?;
        }
        Ok(())
    }

    /// Add one to (`Add`) or remove one from (`Sub`) a box's live-entry count.
    pub(super) fn bump_live_count(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        kind: BinaryKind,
    ) -> Result<()> {
        let sz_op = FieldGetOp::new(ctx, box_val, "_1".to_string())?;
        let sz = self.append_value(ctx, &sz_op);
        let one = self.number(ctx, 1.0);
        let next_sz = self.binary(ctx, kind, sz, one);
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, box_val, "_1".to_string(), next_sz)
        });
        Ok(())
    }

    /// A reference-free placeholder of `ty`; `None` for types holding nothing to release.
    fn zero_value_for_type(&mut self, ctx: &mut Context, ty: TypeHandle) -> Option<Value> {
        if optional_value(ctx, ty).is_some() {
            if absence(ctx, ty).is_some_and(|a| !a.admits_undefined()) {
                return Some(self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty)));
            }
            return Some(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)));
        }
        if ty.deref(ctx).is::<StringType>() {
            return Some(self.string(ctx, ""));
        }
        if ty.deref(ctx).is::<FP64Type>() {
            return Some(self.number(ctx, 0.0));
        }
        if let Ok(int_ty) = TypedHandle::<IntegerType>::from_handle(ty, ctx) {
            if int_ty.deref(ctx).width() == 1 {
                return Some(self.boolean(ctx, false));
            }
            let zero = self.number(ctx, 0.0);
            return Some(self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, zero, ty)));
        }
        if let Ok(arr_ty) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            let elem_ty = arr_ty.deref(ctx).element();
            return Some(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![])));
        }
        None
    }

    /// After a delete: empty the entries when nothing is live, else compact once at least 32
    /// and at least half of the slots are tombstones, moving active cursors with their entries.
    fn reclaim_after_delete(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        kind: EntryKind,
    ) -> Result<()> {
        let entries_op = FieldGetOp::new(ctx, box_val, "_0".to_string())?;
        let entries = self.append_value(ctx, &entries_op);
        let live_op = FieldGetOp::new(ctx, box_val, "_1".to_string())?;
        let live = self.append_value(ctx, &live_op);
        let cursors_op = FieldGetOp::new(ctx, box_val, "_2".to_string())?;
        let cursors = self.append_value(ctx, &cursors_op);

        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let inf = self.number(ctx, f64::INFINITY);
        let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();

        let is_empty = self.compare(ctx, CompareKind::Eq, live, zero);
        let empty_bb = self.new_block(ctx, "reclaim_empty");
        let check_compact_bb = self.new_block(ctx, "reclaim_check");
        let compact_bb = self.new_block(ctx, "reclaim_compact");
        let done_bb = self.new_block(ctx, "reclaim_done");

        self.branch(ctx, is_empty, empty_bb, check_compact_bb);

        self.switch_to(empty_bb);
        let zero_u32 = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, zero, u32_ty));
        self.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, entries, zero_u32));
        self.reset_active_cursors(ctx, "reclaim_reset_cursors", cursors)?;
        self.jump(ctx, done_bb);

        self.switch_to(check_compact_bb);
        let cap = self.array_length(ctx, entries);
        let dead = self.binary(ctx, BinaryKind::Sub, cap, live);
        let min_dead = self.number(ctx, 32.0);
        let enough_dead = self.compare(ctx, CompareKind::Ge, dead, min_dead);
        let half_dead = self.compare(ctx, CompareKind::Ge, dead, live);
        let bool_ty = enough_dead.get_type(ctx);
        let should_compact = self.choose(
            ctx,
            bool_ty,
            enough_dead,
            |_, _| Ok(half_dead),
            |_, _| Ok(enough_dead),
        )?;
        self.branch(ctx, should_compact, compact_bb, done_bb);

        self.switch_to(compact_bb);
        let write_slot = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, write_slot, zero);
        self.index_loop(ctx, "reclaim_scan", zero, cap, |b, ctx, r, next, _| {
            let e = b.element_at(ctx, entries, r)?;
            let live_op = FieldGetOp::new(ctx, e, kind.live_field().to_string())?;
            let is_live = b.append_value(ctx, &live_op);
            let live_branch = b.new_block(ctx, "reclaim_keep");
            let dead_branch = b.new_block(ctx, "reclaim_drop");
            b.branch(ctx, is_live, live_branch, dead_branch);

            b.switch_to(live_branch);
            let w = b.load(ctx, write_slot)?;
            b.emit(ctx, |ctx| ArraySetOp::new(ctx, entries, w, e));
            let next_w = b.binary(ctx, BinaryKind::Add, w, one);
            b.store(ctx, write_slot, next_w);
            b.jump(ctx, next);

            b.switch_to(dead_branch);
            let cur_w = b.load(ctx, write_slot)?;
            let cur_len = b.array_length(ctx, cursors);
            b.index_loop(
                ctx,
                "reclaim_adj_cursors",
                zero,
                cur_len,
                |b, ctx, ci, c_next, _| {
                    let c = b.element_at(ctx, cursors, ci)?;
                    let c_op = FieldGetOp::new(ctx, c, "_0".to_string())?;
                    let c_idx = b.append_value(ctx, &c_op);
                    let gt = b.compare(ctx, CompareKind::Gt, c_idx, cur_w);
                    let active = b.compare(ctx, CompareKind::Lt, c_idx, inf);
                    let bool_ty = gt.get_type(ctx);
                    let should_adj =
                        b.choose(ctx, bool_ty, gt, |_, _| Ok(active), |_, _| Ok(gt))?;
                    let adj_bb = b.new_block(ctx, "reclaim_adj_hit");
                    b.branch(ctx, should_adj, adj_bb, c_next);
                    b.switch_to(adj_bb);
                    let dec = b.binary(ctx, BinaryKind::Sub, c_idx, one);
                    b.emit(ctx, |ctx| FieldSetOp::new(ctx, c, "_0".to_string(), dec));
                    b.jump(ctx, c_next);
                    Ok(())
                },
            )?;
            b.jump(ctx, next);
            Ok(())
        })?;
        let final_w = self.load(ctx, write_slot)?;
        let new_len = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, final_w, u32_ty));
        self.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, entries, new_len));
        self.jump(ctx, done_bb);

        self.switch_to(done_bb);
        Ok(())
    }

    /// Rewind every unexhausted cursor to the first slot of emptied entries.
    fn reset_active_cursors(
        &mut self,
        ctx: &mut Context,
        label: &str,
        cursors: Value,
    ) -> Result<()> {
        let zero = self.number(ctx, 0.0);
        let inf = self.number(ctx, f64::INFINITY);
        let c_len = self.array_length(ctx, cursors);
        let hit_label = format!("{label}_hit");
        self.index_loop(ctx, label, zero, c_len, |b, ctx, ci, c_next, _| {
            let c = b.element_at(ctx, cursors, ci)?;
            let c_op = FieldGetOp::new(ctx, c, "_0".to_string())?;
            let c_idx = b.append_value(ctx, &c_op);
            let active = b.compare(ctx, CompareKind::Lt, c_idx, inf);
            let rst_bb = b.new_block(ctx, &hit_label);
            b.branch(ctx, active, rst_bb, c_next);
            b.switch_to(rst_bb);
            b.emit(ctx, |ctx| FieldSetOp::new(ctx, c, "_0".to_string(), zero));
            b.jump(ctx, c_next);
            Ok(())
        })
    }

    /// The liveness flag of a `Map`/`Set` entry; `None` for entries without one.
    pub(crate) fn collection_entry_live(
        &mut self,
        ctx: &mut Context,
        entry: Value,
    ) -> Result<Option<Value>> {
        let kind = TypedHandle::<RecordType>::from_handle(entry.get_type(ctx), ctx)
            .ok()
            .and_then(|rec| EntryKind::of_entry(ctx, rec));
        let Some(kind) = kind else {
            return Ok(None);
        };
        let op = FieldGetOp::new(ctx, entry, kind.live_field().to_string())?;
        Ok(Some(self.append_value(ctx, &op)))
    }

    /// `size`: a box's live count, otherwise a count of live entries.
    pub(crate) fn collection_size(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        if let Some(box_val) = self.collection_underlying_box(ctx, arr)? {
            let op = FieldGetOp::new(ctx, box_val, "_1".to_string())?;
            return Ok(self.append_value(ctx, &op));
        }
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

    /// `clear()`: empty a box and rewind its cursors, or tombstone every entry of a bare array.
    pub(super) fn collection_clear(&mut self, ctx: &mut Context, coll: Value) -> Result<Value> {
        if let Some(box_val) = self.collection_underlying_box(ctx, coll)? {
            let entries = self.collection_entries_array(ctx, box_val)?;
            let zero = self.number(ctx, 0.0);
            let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
            let zero_u32 = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, zero, u32_ty));
            self.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, entries, zero_u32));
            self.emit(ctx, |ctx| {
                FieldSetOp::new(ctx, box_val, "_1".to_string(), zero)
            });
            let cursors_op = FieldGetOp::new(ctx, box_val, "_2".to_string())?;
            let cursors = self.append_value(ctx, &cursors_op);
            self.reset_active_cursors(ctx, "clear_cursors", cursors)?;
        } else {
            let kind = TypedHandle::<ArrayType>::from_handle(coll.get_type(ctx), ctx)
                .ok()
                .and_then(|arr| {
                    TypedHandle::<RecordType>::from_handle(arr.deref(ctx).element(), ctx).ok()
                })
                .and_then(|rec| EntryKind::of_entry(ctx, rec))
                .context("clear() requires a Map or Set")?;
            let limit = self.array_length(ctx, coll);
            let zero = self.number(ctx, 0.0);
            let false_val = self.boolean(ctx, false);
            self.index_loop(ctx, "coll_clear", zero, limit, |b, ctx, idx, _, _| {
                let entry = b.element_at(ctx, coll, idx)?;
                b.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, entry, kind.live_field().to_string(), false_val)
                });
                Ok(())
            })?;
        }
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    /// Run `body(index, item)` for each live entry of `coll`, in insertion order, tracking the
    /// position with a [LiveCursor] so deletes during `body` neither skip nor repeat entries.
    pub(super) fn live_collection_loop(
        &mut self,
        ctx: &mut Context,
        label: &str,
        coll: Value,
        body: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<()>,
    ) -> Result<()> {
        let entries = self.collection_entries_array(ctx, coll)?;
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let index = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, index, zero);
        let cursor = LiveCursor::open(self, ctx, coll, index)?;
        let header = self.new_block(ctx, &format!("{label}_header"));
        let check = self.new_block(ctx, &format!("{label}_check"));
        let entered = self.new_block(ctx, &format!("{label}_body"));
        let next = self.new_block(ctx, &format!("{label}_next"));
        let done = self.new_block(ctx, &format!("{label}_done"));
        self.jump(ctx, header);
        self.switch_to(header);
        let current = self.load(ctx, index)?;
        let limit = self.array_length(ctx, entries);
        let more = self.compare(ctx, CompareKind::Lt, current, limit);
        self.branch(ctx, more, check, done);
        self.switch_to(check);
        let next_idx = self.binary(ctx, BinaryKind::Add, current, one);
        if let Some(cursor) = cursor {
            cursor.advance(self, ctx, next_idx);
        }
        let raw_entry = self.element_at(ctx, entries, current)?;
        match self.collection_entry_live(ctx, raw_entry)? {
            Some(live) => self.branch(ctx, live, entered, next),
            None => self.jump(ctx, entered),
        }
        self.switch_to(entered);
        let item = self.collection_view_element_at(ctx, coll, current)?;
        body(self, ctx, current, item)?;
        self.jump(ctx, next);
        self.switch_to(next);
        let advanced = match cursor {
            Some(cursor) => cursor.position(self, ctx)?,
            None => {
                let cur = self.load(ctx, index)?;
                self.binary(ctx, BinaryKind::Add, cur, one)
            }
        };
        self.store(ctx, index, advanced);
        self.jump(ctx, header);
        self.switch_to(done);
        if let Some(cursor) = cursor {
            cursor.exhausted(self, ctx)?;
            cursor.exited(self, ctx)?;
        }
        Ok(())
    }
}
