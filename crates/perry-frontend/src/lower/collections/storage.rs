//! Live collection storage: boxes, views, cursors, tombstones, compaction and live iteration.

use super::StorageField as Field;
use super::*;

/// A cursor tracking `(index, min_seq, epoch)` against a collection box.
#[derive(Clone, Copy)]
pub(crate) struct LiveCursor {
    box_val: Value,
    cursor: Value,
    view: CollectionViewKind,
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
        b: &mut FunctionBuilder<'_, '_>,
        ctx: &mut Context,
        coll: Value,
        index: Value,
    ) -> Result<Option<Self>> {
        let Some((layout, box_val)) = b.collection_storage(ctx, coll)? else {
            return Ok(None);
        };
        let owner = if box_val == coll {
            CursorOwner::Loop
        } else {
            CursorOwner::View
        };
        let cursor = match owner {
            CursorOwner::View => b.storage_field(ctx, coll, Field::ViewCursor)?,
            CursorOwner::Loop => {
                let start = b.load(ctx, index)?;
                b.new_collection_cursor(ctx, box_val, start)?
            }
        };
        let live = Self {
            box_val,
            cursor,
            view: layout.view,
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
    fn advance(
        self,
        b: &mut FunctionBuilder<'_, '_>,
        ctx: &mut Context,
        next: Value,
        entry: Value,
    ) -> Result<()> {
        let Self {
            box_val,
            cursor,
            view,
            ..
        } = self;
        let seq = b.storage_field(ctx, entry, Field::Sequence(view.source()))?;
        let one = b.number(ctx, 1.0);
        let next_seq = b.binary(ctx, BinaryKind::Add, seq, one);
        let epoch = b.storage_field(ctx, box_val, Field::BoxEpoch)?;
        b.set_storage_field(ctx, cursor, Field::CursorIndex, next);
        b.set_storage_field(ctx, cursor, Field::CursorSequence, next_seq);
        b.set_storage_field(ctx, cursor, Field::CursorEpoch, epoch);
        Ok(())
    }

    /// The index the next visit starts at, synchronized with clears and compactions.
    pub(crate) fn position(
        self,
        b: &mut FunctionBuilder<'_, '_>,
        ctx: &mut Context,
    ) -> Result<Value> {
        b.sync_collection_cursor(ctx, self.box_val, self.cursor, self.view.source())
    }

    /// The loop ran out of entries: a view stays exhausted at infinity.
    pub(crate) fn exhausted(self, b: &mut FunctionBuilder<'_, '_>, ctx: &mut Context) {
        if matches!(self.owner, CursorOwner::View) {
            let inf = b.number(ctx, f64::INFINITY);
            b.set_storage_field(ctx, self.cursor, Field::CursorIndex, inf);
            b.set_storage_field(ctx, self.cursor, Field::CursorSequence, inf);
        }
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn storage_field(
        &mut self,
        ctx: &mut Context,
        record: Value,
        field: Field,
    ) -> Result<Value> {
        let op = FieldGetOp::new(ctx, record, field.name().to_owned()).to_diag(ctx)?;
        Ok(self.emit_value(op, ctx))
    }

    pub(super) fn set_storage_field(
        &mut self,
        ctx: &mut Context,
        record: Value,
        field: Field,
        value: Value,
    ) {
        self.emit(
            FieldSetOp::new(ctx, record, field.name().to_owned(), value),
            ctx,
        );
    }

    pub(in crate::lower) fn alloc_collection_box(
        &mut self,
        ctx: &mut Context,
        layout: CollectionLayout,
    ) -> Value {
        let entries = self.emit_value(ArrayNewOp::new(ctx, layout.entry.into(), vec![]), ctx);
        let zero = self.number(ctx, 0.0);
        self.emit_value(
            AllocOp::new(ctx, layout.boxed, vec![entries, zero, zero, zero]),
            ctx,
        )
    }

    /// A `view` over `box_val` with its own cursor at the first entry.
    pub(super) fn alloc_collection_view(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        layout: CollectionLayout,
        view: CollectionViewKind,
    ) -> Result<Value> {
        let zero = self.number(ctx, 0.0);
        let cursor = self.new_collection_cursor(ctx, box_val, zero)?;
        let view_rec = self.shapes.collection_view_type(ctx, layout, view)?;
        Ok(self.emit_value(AllocOp::new(ctx, view_rec, vec![box_val, cursor]), ctx))
    }

    pub(super) fn collection_storage(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Option<(CollectionLayout, Value)>> {
        let Some(layout) = self.shapes.collection(ctx, val.get_type(ctx)) else {
            return Ok(None);
        };
        let owner = if TypeHandle::from(layout.boxed) == present_type(ctx, val.get_type(ctx)) {
            val
        } else {
            self.storage_field(ctx, val, Field::ViewBox)?
        };
        Ok(Some((layout, owner)))
    }

    /// Return the entries array of a collection or view, or the array itself for plain arrays.
    pub(crate) fn collection_entries_array(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        match self.collection_storage(ctx, val)? {
            Some((_, box_val)) => self.storage_field(ctx, box_val, Field::BoxEntries),
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
        let Some((layout, source)) = self.collection_storage(ctx, val)? else {
            return self.array_value(ctx, val, idx);
        };
        let entries = self.storage_field(ctx, source, Field::BoxEntries)?;
        let entry = self.element_at(ctx, entries, idx)?;
        self.project_entry(ctx, layout.view, entry)
    }

    fn project_entry(
        &mut self,
        ctx: &mut Context,
        view: CollectionViewKind,
        entry: Value,
    ) -> Result<Value> {
        view.fields()
            .try_map(|field| self.storage_field(ctx, entry, field))?
            .materialize(Ok, |[key, value]| self.pair(ctx, key, value))
    }

    pub(in crate::lower) fn visit_collection_entry(
        &mut self,
        ctx: &mut Context,
        source: Value,
        index: Value,
        cursor: Option<LiveCursor>,
        next: Ptr<BasicBlock>,
    ) -> Result<Value> {
        let (entries, kind) = if let Some(cursor) = cursor {
            (
                self.storage_field(ctx, cursor.box_val, Field::BoxEntries)?,
                cursor.view.source(),
            )
        } else {
            let element = self.array_element(ctx, source)?;
            let Some(kind) =
                perry_mir::typed(ctx, element).and_then(|r| CollectionKind::of_entry(ctx, r))
            else {
                return self.array_value(ctx, source, index);
            };
            (source, kind)
        };
        let entry = self.element_at(ctx, entries, index)?;
        if let Some(cursor) = cursor {
            let one = self.number(ctx, 1.0);
            let advanced = self.binary(ctx, BinaryKind::Add, index, one);
            cursor.advance(self, ctx, advanced, entry)?;
        }
        let live = self.storage_field(ctx, entry, Field::Live(kind))?;
        let visit = self.new_block(ctx, "loop_live");
        self.branch(ctx, live, visit, next);
        self.switch_to(visit);
        match cursor {
            Some(cursor) => self.project_entry(ctx, cursor.view, entry),
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
        let k = self.storage_field(ctx, entry, Field::Key)?;
        let v = self.storage_field(ctx, entry, Field::MapValue)?;
        Ok((k, v))
    }

    /// A fresh `[first, second]` tuple.
    pub(crate) fn pair(&mut self, ctx: &mut Context, first: Value, second: Value) -> Result<Value> {
        let (first_ty, second_ty) = (first.get_type(ctx), second.get_type(ctx));
        let tuple_rec =
            self.shapes
                .positional_record(ctx, RecordKind::Tuple, [first_ty, second_ty])?;
        Ok(self.emit_value(AllocOp::new(ctx, tuple_rec, vec![first, second]), ctx))
    }

    pub(super) fn update_counter(
        &mut self,
        ctx: &mut Context,
        storage: Value,
        field: Field,
        op: BinaryKind,
    ) -> Result<(Value, Value)> {
        let before = self.storage_field(ctx, storage, field)?;
        let one = self.number(ctx, 1.0);
        let after = self.binary(ctx, op, before, one);
        self.set_storage_field(ctx, storage, field, after);
        Ok((before, after))
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
        Ok(self.emit_value(AllocOp::new(ctx, cursor_rec, vec![idx, zero, epoch]), ctx))
    }

    fn sync_collection_cursor(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        cursor: Value,
        kind: CollectionKind,
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
        let new_idx = self.find_item(
            ctx,
            Input::Array(entries, Holes::Skip),
            |_, _| Ok(len),
            |b, ctx, entry, _| {
                let seq = b.storage_field(ctx, entry, Field::Sequence(kind))?;
                Ok(b.compare(ctx, CompareKind::Ge, seq, min_seq))
            },
            |_, _, _, index| Ok(index),
        )?;
        self.set_storage_field(ctx, cursor, Field::CursorIndex, new_idx);
        self.set_storage_field(ctx, cursor, Field::CursorEpoch, box_epoch);
        self.jump(ctx, done_bb);

        self.switch_to(done_bb);
        self.storage_field(ctx, cursor, Field::CursorIndex)
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
                return Some(self.emit_value(NullOp::new(ctx, ty), ctx));
            }
            return Some(self.emit_value(UndefinedOp::with_type(ctx, ty), ctx));
        }
        if ty.deref(ctx).is::<StringType>() {
            return Some(self.string(ctx, ""));
        }
        if let Some(arr_ty) = perry_mir::typed::<ArrayType>(ctx, ty) {
            let elem_ty = arr_ty.deref(ctx).element();
            return Some(self.emit_value(ArrayNewOp::new(ctx, elem_ty, vec![]), ctx));
        }
        None
    }

    /// After a delete: empty the entries when nothing is live, else mark the tombstone and
    /// compact once at least 32 and at least half of the slots are tombstones, bumping the box epoch.
    pub(super) fn delete_entry(
        &mut self,
        ctx: &mut Context,
        box_val: Value,
        entries: Value,
        entry: Value,
        kind: CollectionKind,
    ) -> Result<()> {
        let (_, live) = self.update_counter(ctx, box_val, Field::BoxLength, BinaryKind::Sub)?;
        let zero = self.number(ctx, 0.0);
        let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();

        let is_empty = self.compare(ctx, CompareKind::Eq, live, zero);
        let empty_bb = self.new_block(ctx, "reclaim_empty");
        let check_compact_bb = self.new_block(ctx, "reclaim_check");
        let compact_bb = self.new_block(ctx, "reclaim_compact");
        let done_bb = self.new_block(ctx, "reclaim_done");

        self.branch(ctx, is_empty, empty_bb, check_compact_bb);

        self.switch_to(empty_bb);
        let zero_u32 = self.emit_value(NumericCastOp::new(ctx, zero, u32_ty), ctx);
        self.emit(ArrayTruncateOp::new(ctx, entries, zero_u32), ctx);
        self.update_counter(ctx, box_val, Field::BoxEpoch, BinaryKind::Add)?;
        self.jump(ctx, done_bb);

        self.switch_to(check_compact_bb);
        let dead = self.boolean(ctx, false);
        self.set_storage_field(ctx, entry, Field::Live(kind), dead);
        let record =
            TypedHandle::<RecordType>::from_handle(entry.get_type(ctx), ctx).to_diag(ctx)?;
        let fields: &[Field] = match kind {
            CollectionKind::Map => &[Field::Key, Field::MapValue],
            CollectionKind::Set => &[Field::Key],
        };
        for &field in fields {
            let (_, ty) = record
                .deref(ctx)
                .field(field.name())
                .ok_or_else(|| internal!("missing collection payload"))?;
            if let Some(dummy) = self.zero_value_for_type(ctx, ty) {
                self.set_storage_field(ctx, entry, field, dummy);
            }
        }
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
            b.storage_field(ctx, entry, Field::Live(kind))
        })?;
        self.update_counter(ctx, box_val, Field::BoxEpoch, BinaryKind::Add)?;
        self.jump(ctx, done_bb);

        self.switch_to(done_bb);
        Ok(())
    }

    /// `size`: a box's live count or a plain array's length.
    pub(crate) fn collection_size(&mut self, ctx: &mut Context, arr: Value) -> Result<Value> {
        if let Some((_, box_val)) = self.collection_storage(ctx, arr)? {
            return self.storage_field(ctx, box_val, Field::BoxLength);
        }
        Ok(self.array_length(ctx, arr))
    }

    /// Empty a collection box and invalidate its cursors' cached positions.
    pub(crate) fn collection_clear(&mut self, ctx: &mut Context, coll: Value) -> Result<Value> {
        let (_, box_val) = self.collection_storage(ctx, coll)?.ok_or_else(|| {
            Diagnostic::new(codes::TYPE_MISMATCH, "clear() requires a Map or Set")
        })?;
        let entries = self.storage_field(ctx, box_val, Field::BoxEntries)?;
        let zero = self.number(ctx, 0.0);
        let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let zero_u32 = self.emit_value(NumericCastOp::new(ctx, zero, u32_ty), ctx);
        self.emit(ArrayTruncateOp::new(ctx, entries, zero_u32), ctx);
        self.set_storage_field(ctx, box_val, Field::BoxLength, zero);
        self.update_counter(ctx, box_val, Field::BoxEpoch, BinaryKind::Add)?;
        let ty = void_type(ctx);
        Ok(self.emit_value(UndefinedOp::with_type(ctx, ty), ctx))
    }
}
