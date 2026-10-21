//! Live collection storage: boxes, views, cursors, tombstones, compaction and live iteration.

use super::StorageField as Field;
use super::*;
use crate::control_flow::Projection;

/// The entry and payload fields cleared when a live collection member is deleted.
struct Tombstone<'a> {
    entry: Value,
    kind: EntryKind,
    fields: &'a [(&'a str, TypeHandle)],
}

/// A cursor tracking `(index, min_seq, epoch)` against a collection box.
#[derive(Clone, Copy)]
pub(crate) struct LiveCursor {
    box_val: Value,
    cursor: Value,
    kind: EntryKind,
    owner: CursorOwner,
}

#[derive(Clone, Copy)]
enum CursorOwner {
    View,
    Loop,
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
        let (cursor, owner) = if collection_view_record(ctx, coll.get_type(ctx)).is_some() {
            (
                b.storage_field(ctx, coll, Field::ViewCursor)?,
                CursorOwner::View,
            )
        } else {
            let start = b.load(ctx, index)?;
            (
                b.new_collection_cursor(ctx, box_val, start)?,
                CursorOwner::Loop,
            )
        };
        let live = Self {
            box_val,
            cursor,
            kind,
            owner,
        };
        if matches!(owner, CursorOwner::View) {
            let start = live.position(b, ctx)?;
            b.store(ctx, index, start);
        }
        Ok(Some(live))
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
        let Self {
            box_val,
            cursor,
            kind,
            ..
        } = self;
        let seq_op = FieldGetOp::new(ctx, entry, kind.seq_field().to_string())?;
        let seq = b.append_value(ctx, &seq_op);
        let one = b.number(ctx, 1.0);
        let next_seq = b.binary(ctx, BinaryKind::Add, seq, one);
        let epoch = b.storage_field(ctx, box_val, Field::BoxEpoch)?;
        b.set_storage_field(ctx, cursor, Field::CursorIndex, next);
        b.set_storage_field(ctx, cursor, Field::CursorSequence, next_seq);
        b.set_storage_field(ctx, cursor, Field::CursorEpoch, epoch);
        Ok(())
    }

    /// The index the next visit starts at, synchronized with clears and compactions.
    pub(crate) fn position(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context) -> Result<Value> {
        b.sync_collection_cursor(ctx, self.box_val, self.cursor, self.kind)
    }

    /// The loop ran out of entries: a view stays exhausted at infinity.
    pub(crate) fn exhausted(self, b: &mut FunctionBuilder<'_>, ctx: &mut Context) -> Result<()> {
        if matches!(self.owner, CursorOwner::View) {
            let inf = b.number(ctx, f64::INFINITY);
            b.set_storage_field(ctx, self.cursor, Field::CursorIndex, inf);
            b.set_storage_field(ctx, self.cursor, Field::CursorSequence, inf);
        }
        Ok(())
    }
}

impl<'s> FunctionBuilder<'s> {
    fn storage_field(&mut self, ctx: &mut Context, record: Value, field: Field) -> Result<Value> {
        let op = FieldGetOp::new(ctx, record, field.name().to_owned())?;
        Ok(self.append_value(ctx, &op))
    }

    fn set_storage_field(&mut self, ctx: &mut Context, record: Value, field: Field, value: Value) {
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, record, field.name().to_owned(), value)
        });
    }

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

    /// Return the underlying box of a `Map`, `Set`, or view, or `None` for plain arrays.
    pub(super) fn collection_underlying_box(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Option<Value>> {
        let ty = val.get_type(ctx);
        if map_box_record(ctx, ty).is_some() || set_box_record(ctx, ty).is_some() {
            Ok(Some(val))
        } else if collection_view_record(ctx, ty).is_some() {
            Ok(Some(self.storage_field(ctx, val, Field::ViewBox)?))
        } else {
            Ok(None)
        }
    }

    /// Return the entries array of a collection or view, or the array itself for plain arrays.
    pub(crate) fn collection_entries_array(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        match self.collection_underlying_box(ctx, val)? {
            Some(box_val) => self.storage_field(ctx, box_val, Field::BoxEntries),
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
        let projection = Projection::collection(ctx, val.get_type(ctx))?;
        let entries = self.collection_entries_array(ctx, val)?;
        let entry = self.element_at(ctx, entries, idx)?;
        projection.read(self, ctx, entry)
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
    pub(crate) fn pair(&mut self, ctx: &mut Context, first: Value, second: Value) -> Result<Value> {
        let (first_ty, second_ty) = (first.get_type(ctx), second.get_type(ctx));
        let tuple_rec = self.shapes.pair_type(ctx, first_ty, second_ty)?;
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![first, second])))
    }

    pub(super) fn next_collection_seq(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
    ) -> Result<Value> {
        let seq = self.storage_field(ctx, box_val, Field::BoxSequence)?;
        let one = self.number(ctx, 1.0);
        let next_seq = self.binary(ctx, BinaryKind::Add, seq, one);
        self.set_storage_field(ctx, box_val, Field::BoxSequence, next_seq);
        Ok(seq)
    }

    fn new_collection_cursor(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        idx: Value,
    ) -> Result<Value> {
        let zero = self.number(ctx, 0.0);
        let epoch = self.storage_field(ctx, box_val, Field::BoxEpoch)?;
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
        let idx = self.storage_field(ctx, cursor, Field::CursorIndex)?;
        let inf = self.number(ctx, f64::INFINITY);
        let active = self.compare(ctx, CompareKind::Lt, idx, inf);
        let active_bb = self.new_block(ctx, "sync_cursor_active");
        let resync_bb = self.new_block(ctx, "sync_cursor_resync");
        let done_bb = self.new_block(ctx, "sync_cursor_done");
        self.branch(ctx, active, active_bb, done_bb);

        self.switch_to(active_bb);
        let cur_epoch = self.storage_field(ctx, cursor, Field::CursorEpoch)?;
        let box_epoch = self.storage_field(ctx, box_val, Field::BoxEpoch)?;
        let same_epoch = self.compare(ctx, CompareKind::Eq, cur_epoch, box_epoch);
        self.branch(ctx, same_epoch, done_bb, resync_bb);

        self.switch_to(resync_bb);
        let min_seq = self.storage_field(ctx, cursor, Field::CursorSequence)?;
        let entries = self.storage_field(ctx, box_val, Field::BoxEntries)?;
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
        self.set_storage_field(ctx, cursor, Field::CursorIndex, new_idx);
        self.set_storage_field(ctx, cursor, Field::CursorEpoch, box_epoch);
        self.jump(ctx, done_bb);

        self.switch_to(done_bb);
        self.storage_field(ctx, cursor, Field::CursorIndex)
    }

    fn bump_box_epoch(&mut self, ctx: &mut Context, box_val: Value) -> Result<()> {
        let epoch = self.storage_field(ctx, box_val, Field::BoxEpoch)?;
        let one = self.number(ctx, 1.0);
        let next_epoch = self.binary(ctx, BinaryKind::Add, epoch, one);
        self.set_storage_field(ctx, box_val, Field::BoxEpoch, next_epoch);
        Ok(())
    }

    /// Mark `entry` deleted, overwrite its `cleared` fields so they hold no references, and
    /// compact the owning box.
    pub(super) fn tombstone_entry(
        &mut self,
        ctx: &mut Context,
        entries: Value,
        entry: Value,
        kind: EntryKind,
        cleared: &[(&str, TypeHandle)],
        box_val: Value,
    ) -> Result<()> {
        let tombstone = Tombstone {
            entry,
            kind,
            fields: cleared,
        };
        let live = self.bump_live_count(ctx, box_val, BinaryKind::Sub)?;
        self.reclaim_after_delete(ctx, box_val, entries, live, tombstone)
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
        let sz = self.storage_field(ctx, box_val, Field::BoxLength)?;
        let one = self.number(ctx, 1.0);
        let next_sz = self.binary(ctx, kind, sz, one);
        self.set_storage_field(ctx, box_val, Field::BoxLength, next_sz);
        Ok(next_sz)
    }

    /// Emit a reference-free placeholder of `ty` when `ty` holds references, or `None` for scalar types.
    pub(crate) fn zero_value_for_type(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
    ) -> Option<Value> {
        let inner = present_type(ctx, ty);
        if inner.deref(ctx).is::<FP64Type>()
            || inner.deref(ctx).is::<IntegerType>()
            || inner.deref(ctx).is::<BigIntType>()
        {
            return None;
        }
        if let Some(states) = absence(ctx, ty) {
            if !states.admits_undefined() {
                return Some(self.emit_value(ctx, |ctx| NullOp::new(ctx, ty)));
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
        self.retain_array(ctx, entries, |b, ctx, entry| {
            let op = FieldGetOp::new(ctx, entry, tombstone.kind.live_field().to_string())?;
            Ok(b.append_value(ctx, &op))
        })?;
        self.bump_box_epoch(ctx, box_val)?;
        self.jump(ctx, done_bb);

        self.switch_to(done_bb);
        Ok(())
    }

    /// Load the liveness flag of a `Map` or `Set` entry when the entry carries one.
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

    /// `size`: a box's live count or a plain array's length.
    pub(crate) fn collection_size(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        if let Some(box_val) = self.collection_underlying_box(ctx, arr)? {
            return self.storage_field(ctx, box_val, Field::BoxLength);
        }
        Ok(self.array_length(ctx, arr))
    }

    /// Empty a collection box and invalidate its cursors' cached positions.
    pub(crate) fn collection_clear(&mut self, ctx: &mut Context, coll: Value) -> Result<Value> {
        let box_val = self
            .collection_underlying_box(ctx, coll)?
            .context("clear() requires a Map or Set")?;
        let entries = self.collection_entries_array(ctx, box_val)?;
        let zero = self.number(ctx, 0.0);
        let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let zero_u32 = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, zero, u32_ty));
        self.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, entries, zero_u32));
        self.set_storage_field(ctx, box_val, Field::BoxLength, zero);
        self.bump_box_epoch(ctx, box_val)?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }
}
