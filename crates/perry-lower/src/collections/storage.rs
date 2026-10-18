//! Live collection storage: boxes, views, cursors, tombstones, compaction and live iteration.

use super::*;

/// The entry and payload fields cleared when a live collection member is deleted.
struct Tombstone<'a> {
    entry: Value,
    kind: EntryKind,
    fields: &'a [(&'a str, TypeHandle)],
}

/// A cursor tracking `(index, min_seq, epoch)` against a collection box.
#[derive(Clone, Copy)]
pub(crate) enum LiveCursor {
    /// A view's own cursor: it survives `break`, and exhaustion parks it at infinity.
    View {
        box_val: Value,
        cursor: Value,
        kind: EntryKind,
    },
    /// A cursor created for one direct collection loop.
    Loop {
        box_val: Value,
        cursor: Value,
        kind: EntryKind,
    },
}

impl LiveCursor {
    /// Start iterating `coll` at the index stored in `index`, or at a view's own position,
    /// which is stored into `index`. `None` when `coll` is not a collection box or view.
    pub(crate) fn open(
        b: &mut FunctionBuilder<'_>,
        ctx: &mut Context,
        coll: Value,
        index: Value,
    ) -> Result<Option<Self>> {
        let Some(box_val) = b.collection_underlying_box(ctx, coll)? else {
            return Ok(None);
        };
        let kind = if map_box_record(ctx, box_val.get_type(ctx)).is_some() {
            EntryKind::Map
        } else {
            EntryKind::Set
        };
        Ok(Some(
            if collection_view_record(ctx, coll.get_type(ctx)).is_some() {
                let op = FieldGetOp::new(ctx, coll, "_1".to_string())?;
                let cursor = b.append_value(ctx, &op);
                let live = Self::View {
                    box_val,
                    cursor,
                    kind,
                };
                let start = live.position(b, ctx)?;
                b.store(ctx, index, start);
                live
            } else {
                let start = b.load(ctx, index)?;
                let cursor = b.new_collection_cursor(ctx, box_val, start)?;
                Self::Loop {
                    box_val,
                    cursor,
                    kind,
                }
            },
        ))
    }

    /// Record that the next visit starts at `next` (after `entry`'s sequence number), before the
    /// body can delete entries.
    pub(crate) fn advance(
        self,
        b: &mut FunctionBuilder<'_>,
        ctx: &mut Context,
        next: Value,
        entry: Value,
    ) -> Result<()> {
        let (Self::View {
            box_val,
            cursor,
            kind,
        }
        | Self::Loop {
            box_val,
            cursor,
            kind,
        }) = self;
        let seq_op = FieldGetOp::new(ctx, entry, kind.seq_field().to_string())?;
        let seq = b.append_value(ctx, &seq_op);
        let one = b.number(ctx, 1.0);
        let next_seq = b.binary(ctx, BinaryKind::Add, seq, one);
        let epoch_op = FieldGetOp::new(ctx, box_val, "_3".to_string())?;
        let epoch = b.append_value(ctx, &epoch_op);
        b.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, cursor, "_0".to_string(), next)
        });
        b.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, cursor, "_1".to_string(), next_seq)
        });
        b.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, cursor, "_2".to_string(), epoch)
        });
        Ok(())
    }

    /// The index the next visit starts at, synchronized with clears and compactions.
    pub(crate) fn position(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context) -> Result<Value> {
        let (Self::View {
            box_val,
            cursor,
            kind,
        }
        | Self::Loop {
            box_val,
            cursor,
            kind,
        }) = self;
        b.sync_collection_cursor(ctx, box_val, cursor, kind)
    }

    /// The loop ran out of entries: a view stays exhausted at infinity.
    pub(crate) fn exhausted(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context) -> Result<()> {
        match self {
            Self::View { cursor, .. } => {
                let inf = b.number(ctx, f64::INFINITY);
                b.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, cursor, "_0".to_string(), inf)
                });
                b.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, cursor, "_1".to_string(), inf)
                });
                Ok(())
            }
            Self::Loop { .. } => Ok(()),
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
        let entries = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, entry.into(), vec![]));
        let zero = self.number(ctx, 0.0);
        Ok(self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, box_rec, vec![entries, zero, zero, zero])
        }))
    }

    /// A `view` over `box_val` with its own cursor at the first entry.
    pub(super) fn alloc_collection_view(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        view: CollectionViewKind,
    ) -> Result<Value> {
        let zero = self.number(ctx, 0.0);
        let cursor = self.new_collection_cursor(ctx, box_val, zero)?;
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
        if entry.get_type(ctx).deref(ctx).is::<ArrayType>() {
            let zero = self.number(ctx, 0.0);
            let one = self.number(ctx, 1.0);
            let k = self.element_at(ctx, entry, zero)?;
            let v = self.element_at(ctx, entry, one)?;
            return Ok((k, v));
        }
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

    pub(super) fn next_collection_seq(
        &mut self,
        ctx: &mut Context,
        owner: Option<Value>,
    ) -> Result<Value> {
        let Some(box_val) = owner else {
            return Ok(self.number(ctx, 0.0));
        };
        let seq_op = FieldGetOp::new(ctx, box_val, "_2".to_string())?;
        let seq = self.append_value(ctx, &seq_op);
        let one = self.number(ctx, 1.0);
        let next_seq = self.binary(ctx, BinaryKind::Add, seq, one);
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, box_val, "_2".to_string(), next_seq)
        });
        Ok(seq)
    }

    fn new_collection_cursor(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        idx: Value,
    ) -> Result<Value> {
        let zero = self.number(ctx, 0.0);
        let epoch_op = FieldGetOp::new(ctx, box_val, "_3".to_string())?;
        let epoch = self.append_value(ctx, &epoch_op);
        let cursor_rec = self.shapes.cursor_tuple_type(ctx)?;
        Ok(self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, cursor_rec, vec![idx, zero, epoch])
        }))
    }

    fn sync_collection_cursor(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        cursor: Value,
        kind: EntryKind,
    ) -> Result<Value> {
        let idx_op = FieldGetOp::new(ctx, cursor, "_0".to_string())?;
        let idx = self.append_value(ctx, &idx_op);
        let inf = self.number(ctx, f64::INFINITY);
        let active = self.compare(ctx, CompareKind::Lt, idx, inf);
        let active_bb = self.new_block(ctx, "sync_cursor_active");
        let resync_bb = self.new_block(ctx, "sync_cursor_resync");
        let done_bb = self.new_block(ctx, "sync_cursor_done");
        self.branch(ctx, active, active_bb, done_bb);

        self.switch_to(active_bb);
        let cur_epoch_op = FieldGetOp::new(ctx, cursor, "_2".to_string())?;
        let cur_epoch = self.append_value(ctx, &cur_epoch_op);
        let box_epoch_op = FieldGetOp::new(ctx, box_val, "_3".to_string())?;
        let box_epoch = self.append_value(ctx, &box_epoch_op);
        let same_epoch = self.compare(ctx, CompareKind::Eq, cur_epoch, box_epoch);
        self.branch(ctx, same_epoch, done_bb, resync_bb);

        self.switch_to(resync_bb);
        let min_seq_op = FieldGetOp::new(ctx, cursor, "_1".to_string())?;
        let min_seq = self.append_value(ctx, &min_seq_op);
        let entries_op = FieldGetOp::new(ctx, box_val, "_0".to_string())?;
        let entries = self.append_value(ctx, &entries_op);
        let len = self.array_length(ctx, entries);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let new_idx_slot = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, new_idx_slot, zero);
        self.index_loop(
            ctx,
            "sync_cursor_scan",
            zero,
            len,
            |b, ctx, i, next, scan_done| {
                let e = b.element_at(ctx, entries, i)?;
                let seq_op = FieldGetOp::new(ctx, e, kind.seq_field().to_string())?;
                let e_seq = b.append_value(ctx, &seq_op);
                let reached = b.compare(ctx, CompareKind::Ge, e_seq, min_seq);
                let step_bb = b.new_block(ctx, "sync_cursor_step");
                b.branch(ctx, reached, scan_done, step_bb);

                b.switch_to(step_bb);
                let next_i = b.binary(ctx, BinaryKind::Add, i, one);
                b.store(ctx, new_idx_slot, next_i);
                b.jump(ctx, next);
                Ok(())
            },
        )?;
        let new_idx = self.load(ctx, new_idx_slot)?;
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, cursor, "_0".to_string(), new_idx)
        });
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, cursor, "_2".to_string(), box_epoch)
        });
        self.jump(ctx, done_bb);

        self.switch_to(done_bb);
        let final_op = FieldGetOp::new(ctx, cursor, "_0".to_string())?;
        Ok(self.append_value(ctx, &final_op))
    }

    fn bump_box_epoch(&mut self, ctx: &mut Context, box_val: Value) -> Result<()> {
        let epoch_op = FieldGetOp::new(ctx, box_val, "_3".to_string())?;
        let epoch = self.append_value(ctx, &epoch_op);
        let one = self.number(ctx, 1.0);
        let next_epoch = self.binary(ctx, BinaryKind::Add, epoch, one);
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, box_val, "_3".to_string(), next_epoch)
        });
        Ok(())
    }

    /// Mark `entry` deleted, overwrite its `cleared` fields so they hold no references, and
    /// compact the owning box, if any.
    pub(super) fn tombstone_entry(
        &mut self,
        ctx: &mut Context,
        entries: Value,
        entry: Value,
        kind: EntryKind,
        cleared: &[(&str, TypeHandle)],
        owner: Option<Value>,
    ) -> Result<()> {
        let tombstone = Tombstone {
            entry,
            kind,
            fields: cleared,
        };
        if let Some(box_val) = owner {
            let live = self.bump_live_count(ctx, box_val, BinaryKind::Sub)?;
            return self.reclaim_after_delete(ctx, box_val, entries, live, tombstone);
        }
        self.clear_tombstone_entry(ctx, &tombstone);
        Ok(())
    }

    fn clear_tombstone_entry(&mut self, ctx: &mut Context, tombstone: &Tombstone<'_>) {
        let Tombstone {
            entry,
            kind,
            fields,
        } = *tombstone;
        let dead = self.boolean(ctx, false);
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, entry, kind.live_field().to_string(), dead)
        });
        for &(field, ty) in fields {
            if let Some(dummy) = self.zero_value_for_type(ctx, ty) {
                self.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, entry, field.to_string(), dummy)
                });
            }
        }
    }

    /// Add one to (`Add`) or remove one from (`Sub`) a box's live-entry count.
    pub(super) fn bump_live_count(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        kind: BinaryKind,
    ) -> Result<Value> {
        let sz_op = FieldGetOp::new(ctx, box_val, "_1".to_string())?;
        let sz = self.append_value(ctx, &sz_op);
        let one = self.number(ctx, 1.0);
        let next_sz = self.binary(ctx, kind, sz, one);
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, box_val, "_1".to_string(), next_sz)
        });
        Ok(next_sz)
    }

    /// A reference-free placeholder of `ty`; `None` for types holding nothing to release.
    fn zero_value_for_type(&mut self, ctx: &mut Context, ty: TypeHandle) -> Option<Value> {
        let inner = optional_value(ctx, ty).unwrap_or(ty);
        if inner.deref(ctx).is::<FP64Type>()
            || inner.deref(ctx).is::<IntegerType>()
            || inner.deref(ctx).is::<BigIntType>()
        {
            return None;
        }
        if optional_value(ctx, ty).is_some() {
            if absence(ctx, ty).is_some_and(|a| !a.admits_undefined()) {
                return Some(self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty)));
            }
            return Some(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)));
        }
        if ty.deref(ctx).is::<StringType>() {
            return Some(self.string(ctx, ""));
        }
        if let Ok(arr_ty) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            let elem_ty = arr_ty.deref(ctx).element();
            return Some(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![])));
        }
        None
    }

    /// After a delete: empty the entries when nothing is live, else mark the tombstone and
    /// compact once at least 32 and at least half of the slots are tombstones, bumping the box epoch.
    fn reclaim_after_delete(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        entries: Value,
        live: Value,
        tombstone: Tombstone<'_>,
    ) -> Result<()> {
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
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
        self.bump_box_epoch(ctx, box_val)?;
        self.jump(ctx, done_bb);

        self.switch_to(check_compact_bb);
        self.clear_tombstone_entry(ctx, &tombstone);
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
            let live_op = FieldGetOp::new(ctx, e, tombstone.kind.live_field().to_string())?;
            let is_live = b.append_value(ctx, &live_op);
            let live_branch = b.new_block(ctx, "reclaim_keep");
            b.branch(ctx, is_live, live_branch, next);

            b.switch_to(live_branch);
            let w = b.load(ctx, write_slot)?;
            b.emit(ctx, |ctx| ArraySetOp::new(ctx, entries, w, e));
            let next_w = b.binary(ctx, BinaryKind::Add, w, one);
            b.store(ctx, write_slot, next_w);
            b.jump(ctx, next);
            Ok(())
        })?;
        let final_w = self.load(ctx, write_slot)?;
        let new_len = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, final_w, u32_ty));
        self.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, entries, new_len));
        self.bump_box_epoch(ctx, box_val)?;
        self.jump(ctx, done_bb);

        self.switch_to(done_bb);
        Ok(())
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

    /// `clear()`: empty a box and bump its epoch, or tombstone every entry of a bare array.
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
            self.bump_box_epoch(ctx, box_val)?;
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
        let raw_entry = self.element_at(ctx, entries, current)?;
        if let Some(cursor) = cursor {
            cursor.advance(self, ctx, next_idx, raw_entry)?;
        }
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
        }
        Ok(())
    }
}
