//! Records, arrays and variants: allocation, field and element access.
use crate::{
    function::FunctionLowering,
    repr::{Carriers, Repr, optional_inner, repr, slot_kind},
    runtime::Runtime,
};
use anyhow::{Result, anyhow};
use perry_abi::layout::{array as layout, record, variant};
use perry_mir::{
    attrs::ErrorKind,
    collections::{
        ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
        ArrayTruncateOp, ConcatSegmentsOp,
    },
    heap::{AllocOp, FieldGetOp, FieldPresentOp, FieldSetOp},
    types::{ArrayType, RecordType},
    variants::{VariantNewOp, VariantPayloadOp, VariantTagOp},
};
use pliron::{
    builtin::types::FP64Type,
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_, '_> {
    pub(crate) fn array_concat(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        let element = Self::element_type(ctx, op.deref(ctx).get_result(0))?;
        let operands = self.operands(ctx, op)?;
        let references = self.i32_const(ctx, i32::from(repr(ctx, element)? == Repr::Ref));
        let reuse = Operation::get_op::<perry_mir::collections::ConcatReuseOp>(op, ctx)
            .map_or_else(Vec::new, |concat| concat.candidates(ctx).0);
        let value = if operands.len() == 2 {
            let mask = reuse.iter().fold(0, |mask, index| mask | (1 << index));
            let reuse = self.i32_const(ctx, mask);
            self.runtime(
                ctx,
                Runtime::ArrayConcat,
                vec![operands[0], operands[1], references, reuse],
            )
        } else {
            let (sources, mark) = if self.state.collectible {
                let mark = self.publish_roots(ctx, &operands);
                let stack = self.runtime(ctx, Runtime::RootsState, vec![]);
                let data = self.load(ctx, MemKind::I32, stack, perry_abi::layout::roots::DATA);
                let two = self.i32_const(ctx, 2);
                let offset = self.num(ctx, NumOpcode::I32Shl, vec![mark, two]);
                (
                    self.num(ctx, NumOpcode::I32Add, vec![data, offset]),
                    Some(mark),
                )
            } else {
                let size = self.i32_const(ctx, (operands.len() * 4) as i32);
                let sources = self.alloc(ctx, size);
                for (index, value) in operands.iter().copied().enumerate() {
                    self.store(ctx, MemKind::I32, sources, index as u32 * 4, value);
                }
                (sources, None)
            };
            let count = self.i32_const(ctx, operands.len() as i32);
            let indices = reuse
                .iter()
                .flat_map(|index| index.to_le_bytes())
                .collect::<Vec<_>>();
            let address = if indices.is_empty() {
                0
            } else {
                self.state.data.append(4, &indices)
            };
            let candidates = self.i32_const(ctx, address as i32);
            let candidate_count = self.i32_const(ctx, reuse.len() as i32);
            let value = self.runtime(
                ctx,
                Runtime::ArrayConcatMany,
                vec![sources, count, references, candidates, candidate_count],
            );
            if let Some(mark) = mark {
                self.reset_roots(ctx, mark);
            }
            value
        };
        self.define(ctx, op, value);
        Ok(())
    }
    /// Lower a record, array or variant op; `false` if `op` is none.
    pub(crate) fn aggregate_op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<bool> {
        if let Some(variant) = Operation::get_op::<VariantNewOp>(op, ctx) {
            let payload = if op.deref(ctx).get_num_operands() == 0 {
                None
            } else {
                let ty = op.deref(ctx).get_operand(0).get_type(ctx);
                Some((ty, self.operand(ctx, op, 0)?))
            };
            let value = self.variant_new(ctx, &variant.tag(ctx), payload)?;
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<VariantTagOp>(op, ctx).is_some() {
            let object = self.operand(ctx, op, 0)?;
            let value = self.load(ctx, MemKind::I32, object, variant::TAG);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<VariantPayloadOp>(op, ctx).is_some() {
            let object = self.operand(ctx, op, 0)?;
            let ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let result = op.deref(ctx).get_result(0).get_type(ctx);
            let value = self.variant_payload(ctx, ty, object, result)?;
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<AllocOp>(op, ctx).is_some() {
            let record_ty = op.deref(ctx).get_result(0).get_type(ctx);
            let record = TypedHandle::<RecordType>::from_handle(record_ty, ctx)
                .map_err(|_| anyhow!("expected a record"))?;
            let fields: Vec<_> = record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .iter()
                .map(|(_, ty)| *ty)
                .collect();
            let size = self.i32_const(ctx, record::field(fields.len()) as i32);
            let object = if let Some(&offset) = self.frame_slots.get(&op) {
                let base = self
                    .local_frame
                    .ok_or_else(|| anyhow!("frame-resident record without frame storage"))?;
                let offset = self.i32_const(ctx, offset as i32);
                self.num(ctx, NumOpcode::I32Add, vec![base, offset])
            } else {
                self.record_storage(ctx, record_ty, size)?
            };
            self.record_header(ctx, object, record);
            for (index, ty) in fields.into_iter().enumerate() {
                let kind = slot_kind(ctx, ty)?;
                let operand = op.deref(ctx).get_operand(index);
                let value = self.mapped_value(ctx, operand)?;
                let slot = self.prepare_slot_value(ctx, value);
                self.store(ctx, kind, object, record::field(index), slot);
            }
            self.define(ctx, op, object);
            return Ok(true);
        }
        if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let (offset, ty) = Self::field_slot(ctx, record, &get.field(ctx))?;
            let kind = slot_kind(ctx, ty)?;
            let object = self.mapped(ctx, record)?;
            let raw = self.load(ctx, kind, object, offset);
            let mut value = self.unpack_slot_value(ctx, raw, ty);
            if perry_mir::optional::absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
                let omitted = self.is_omitted_value(ctx, value, ty)?;
                let undef = self.undefined_value(ctx, ty)?;
                value = self.select_carriers(ctx, omitted, undef, value)?;
            }
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if let Some(present) = Operation::get_op::<FieldPresentOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let (offset, ty) = Self::field_slot(ctx, record, &present.field(ctx))?;
            let value =
                if perry_mir::optional::absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
                    let kind = slot_kind(ctx, ty)?;
                    let object = self.mapped(ctx, record)?;
                    let raw = self.load(ctx, kind, object, offset);
                    let unpacked = self.unpack_slot_value(ctx, raw, ty);
                    let omitted = self.is_omitted_value(ctx, unpacked, ty)?;
                    self.num(ctx, NumOpcode::I32Eqz, vec![omitted])
                } else {
                    self.i32_const(ctx, 1)
                };
            self.define(ctx, op, value);
            return Ok(true);
        }
        if let Some(set) = Operation::get_op::<FieldSetOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let val_opd = op.deref(ctx).get_operand(1);
            let (offset, ty) = Self::field_slot(ctx, record, &set.field(ctx))?;
            let kind = slot_kind(ctx, ty)?;
            let object = self.mapped(ctx, record)?;
            let value = self.mapped_value(ctx, val_opd)?;
            let slot = self.prepare_slot_value(ctx, value);
            self.store(ctx, kind, object, offset, slot);
            return Ok(true);
        }
        if Operation::get_op::<ArrayNewOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_result(0))?;
            let kind = slot_kind(ctx, element)?;
            let count = op.deref(ctx).get_num_operands();
            let capacity = self.i32_const(ctx, count as i32);
            let array = self.new_array(ctx, capacity, element)?;
            if count > 0 {
                let mut slots = Vec::with_capacity(count);
                for index in 0..count {
                    let operand = op.deref(ctx).get_operand(index);
                    let value = self.mapped_value(ctx, operand)?;
                    slots.push(self.prepare_slot_value(ctx, value));
                }
                let data = self.load(ctx, MemKind::I32, array, layout::DATA);
                for (index, slot) in slots.into_iter().enumerate() {
                    self.store(ctx, kind, data, layout::SLOT * index as u32, slot);
                }
                let len = self.i32_const(ctx, count as i32);
                self.store(ctx, MemKind::I32, array, layout::LEN, len);
            }
            self.define(ctx, op, array);
            return Ok(true);
        }
        if Operation::get_op::<ArrayLenOp>(op, ctx).is_some() {
            let array = self.operands(ctx, op)?[0];
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            self.define(ctx, op, len);
            return Ok(true);
        }
        if Operation::get_op::<ArrayTruncateOp>(op, ctx).is_some() {
            let operands = self.operands(ctx, op)?;
            self.store(ctx, MemKind::I32, operands[0], layout::LEN, operands[1]);
            return Ok(true);
        }
        if let Some(native) = Operation::get_op::<perry_mir::collections::NativeJsonOp>(op, ctx) {
            use pliron::builtin::op_interfaces::OneOpdInterface;
            let source = native.get_operand(ctx);
            let descriptor = self.native_descriptor(ctx, source.get_type(ctx))?;
            let descriptor = self.i32_const(ctx, descriptor as i32);
            let source = self.mapped(ctx, source)?;
            let value = self.runtime(ctx, Runtime::StdJsonNative, vec![source, descriptor]);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ConcatSegmentsOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_result(0))?;
            let sources = self.operand(ctx, op, 0)?;
            let references = self.i32_const(ctx, i32::from(repr(ctx, element)? == Repr::Ref));
            let value = self.runtime(ctx, Runtime::ArrayConcatSegments, vec![sources, references]);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ArrayExtendOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let operands = self.operands(ctx, op)?;
            let references = self.i32_const(ctx, i32::from(repr(ctx, element)? == Repr::Ref));
            self.runtime_void(
                ctx,
                Runtime::ArrayExtend,
                vec![operands[0], operands[1], references],
            );
            return Ok(true);
        }
        if Operation::get_op::<ArrayPushOp>(op, ctx).is_some() {
            let arr_opd = op.deref(ctx).get_operand(0);
            let val_opd = op.deref(ctx).get_operand(1);
            let element = Self::element_type(ctx, arr_opd)?;
            let kind = slot_kind(ctx, element)?;
            let array = self.mapped(ctx, arr_opd)?;
            let value = self.mapped_value(ctx, val_opd)?;
            let slot = self.prepare_slot_value(ctx, value);
            self.push(
                ctx,
                array,
                slot,
                kind,
                repr(ctx, element)? == Repr::Ref,
            );
            return Ok(true);
        }
        if Operation::get_op::<ArrayGetOp>(op, ctx).is_some() {
            // Branch-free: read slot 0 when out of range (capacity is never 0), then select.
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let kind = slot_kind(ctx, element)?;
            let operands = self.operands(ctx, op)?;
            let (array, index) = (operands[0], operands[1]);
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, _) = self.element_index(ctx, index, len)?;
            let zero = self.i32_const(ctx, 0);
            let slot = self.select(ctx, ok, i, zero);
            let data = self.load(ctx, MemKind::I32, array, layout::DATA);
            let address = self.slot_address(ctx, data, slot);
            let loaded_raw = self.load(ctx, kind, address, 0);
            let loaded = self.unpack_slot_value(ctx, loaded_raw, element);
            let target = op.deref(ctx).get_result(0).get_type(ctx);
            let loaded = if optional_inner(ctx, element).is_some() {
                self.optional_cast_value(ctx, loaded, element, target)?
            } else {
                loaded
            };
            let undefined = self.undefined_value(ctx, target)?;
            let value = self.select_carriers(ctx, ok, loaded, undefined)?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ArrayAtOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let kind = slot_kind(ctx, element)?;
            let operands = self.operands(ctx, op)?;
            let data = self.load(ctx, MemKind::I32, operands[0], layout::DATA);
            let address = self.slot_address(ctx, data, operands[1]);
            let raw = self.load(ctx, kind, address, 0);
            let value = self.unpack_slot_value(ctx, raw, element);
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
            let arr_opd = op.deref(ctx).get_operand(0);
            let idx_opd = op.deref(ctx).get_operand(1);
            let val_opd = op.deref(ctx).get_operand(2);
            let element = Self::element_type(ctx, arr_opd)?;
            let kind = slot_kind(ctx, element)?;
            let array = self.mapped(ctx, arr_opd)?;
            let index = self.mapped(ctx, idx_opd)?;
            let value = self.mapped_value(ctx, val_opd)?;
            let slot = self.prepare_slot_value(ctx, value);
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, integral) = self.element_index(ctx, index, len)?;
            let write = self.new_block(ctx, vec![]);
            let outside = self.new_block(ctx, vec![]);
            let done = self.new_block(ctx, vec![]);
            self.cond_br(ctx, ok, (write, vec![]), (outside, vec![]));
            self.block = write;
            let data = self.load(ctx, MemKind::I32, array, layout::DATA);
            let address = self.slot_address(ctx, data, i);
            self.store(ctx, kind, address, 0, slot);
            self.br(ctx, done, vec![]);
            // Writing at `length` appends; any other index would make a hole.
            self.block = outside;
            let at_end = self.num(ctx, NumOpcode::I32Eq, vec![i, len]);
            let append = match integral {
                Some(integral) => self.num(ctx, NumOpcode::I32And, vec![at_end, integral]),
                None => at_end,
            };
            let hole = self.num(ctx, NumOpcode::I32Eqz, vec![append]);
            self.throw_if(ctx, hole, ErrorKind::RangeError);
            self.push(ctx, array, slot, kind, repr(ctx, element)? == Repr::Ref);
            self.br(ctx, done, vec![]);
            self.block = done;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn prepare_slot_value(&mut self, ctx: &mut Context, value: Carriers<Value>) -> Value {
        match value {
            Carriers::Scalar(v) => v,
            Carriers::Numeric { tag, payload } => {
                let size = self.i32_const(ctx, 16);
                let addr = self.alloc(ctx, size);
                self.store(ctx, MemKind::I32, addr, 0, tag);
                self.store(ctx, MemKind::I64, addr, 8, payload);
                addr
            }
        }
    }

    pub(crate) fn unpack_slot_value(
        &mut self,
        ctx: &mut Context,
        raw: Value,
        ty: TypeHandle,
    ) -> Carriers<Value> {
        if crate::values::is_numeric_type(ctx, ty) {
            let tag = self.load(ctx, MemKind::I32, raw, 0);
            let payload = self.load(ctx, MemKind::I64, raw, 8);
            Carriers::Numeric { tag, payload }
        } else {
            Carriers::Scalar(raw)
        }
    }

    pub(crate) fn select_carriers(
        &mut self,
        ctx: &mut Context,
        cond: Value,
        a: Carriers<Value>,
        b: Carriers<Value>,
    ) -> Result<Carriers<Value>> {
        Ok(a.zip(b)?.map(|(av, bv)| self.select(ctx, cond, av, bv)))
    }

    /// Element address and validity for `index` (f64 or i32) in an array of length `len`.
    fn element_index(
        &mut self,
        ctx: &mut Context,
        index: Value,
        len: Value,
    ) -> Result<(Value, Value, Option<Value>)> {
        if index.get_type(ctx).deref(ctx).is::<FP64Type>() {
            let i = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![index]);
            let back = self.num(ctx, NumOpcode::F64ConvertI32U, vec![i]);
            let integral = self.num(ctx, NumOpcode::F64Eq, vec![back, index]);
            let below = self.num(ctx, NumOpcode::I32LtU, vec![i, len]);
            let ok = self.num(ctx, NumOpcode::I32And, vec![integral, below]);
            Ok((i, ok, Some(integral)))
        } else {
            let ok = self.num(ctx, NumOpcode::I32LtU, vec![index, len]);
            Ok((index, ok, None))
        }
    }

    pub(crate) fn slot_address(&mut self, ctx: &mut Context, data: Value, index: Value) -> Value {
        let three = self.i32_const(ctx, 3);
        let offset = self.num(ctx, NumOpcode::I32Shl, vec![index, three]);
        self.num(ctx, NumOpcode::I32Add, vec![data, offset])
    }

    /// Append `value` to `array`, growing it through the runtime when full.
    fn push(
        &mut self,
        ctx: &mut Context,
        array: Value,
        value: Value,
        kind: MemKind,
        references: bool,
    ) {
        let len = self.load(ctx, MemKind::I32, array, layout::LEN);
        let cap = self.load(ctx, MemKind::I32, array, layout::CAP);
        let full = self.num(ctx, NumOpcode::I32Eq, vec![len, cap]);
        let grow = self.new_block(ctx, vec![]);
        let write = self.new_block(ctx, vec![]);
        self.cond_br(ctx, full, (grow, vec![]), (write, vec![]));
        self.block = grow;
        if self.state.collectible {
            let references = self.i32_const(ctx, i32::from(references));
            self.runtime_void(ctx, Runtime::ArrayGrowCollectible, vec![array, references]);
        } else {
            self.runtime_void(ctx, Runtime::ArrayGrow, vec![array]);
        }
        self.br(ctx, write, vec![]);
        self.block = write;
        let data = self.load(ctx, MemKind::I32, array, layout::DATA);
        let address = self.slot_address(ctx, data, len);
        self.store(ctx, kind, address, 0, value);
        let one = self.i32_const(ctx, 1);
        let next = self.num(ctx, NumOpcode::I32Add, vec![len, one]);
        self.store(ctx, MemKind::I32, array, layout::LEN, next);
    }

    pub(crate) fn element_type(ctx: &Context, array: Value) -> Result<TypeHandle> {
        let ty = TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected an array"))?;
        Ok(ty.deref(ctx).element())
    }

    fn field_slot(ctx: &Context, record: Value, field: &str) -> Result<(u32, TypeHandle)> {
        let ty = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected a record"))?;
        let ty = ty.deref(ctx);
        let (index, field_ty) = ty
            .field(field)
            .ok_or_else(|| anyhow!("record has no field {field}"))?;
        Ok((record::field(index), field_ty))
    }

    /// Initialize the header of fresh record storage: the id of shape `record` and, when the
    /// module retains parsed JSON, an empty source word.
    pub(crate) fn record_header(
        &mut self,
        ctx: &mut Context,
        object: Value,
        record: TypedHandle<RecordType>,
    ) {
        let shape = {
            let record = record.deref(ctx);
            let name: &str = record.name().as_ref();
            if let Some(&shape) = self.state.shapes.get(name) {
                shape
            } else {
                let next_shape = self.state.shapes.len() as i32 + 1;
                self.state.shapes.insert(name.to_string(), next_shape);
                next_shape
            }
        };
        if self.state.retains_json {
            let zero = self.i32_const(ctx, 0);
            self.store(ctx, MemKind::I32, object, record::JSON, zero);
        }
        let shape = self.i32_const(ctx, shape);
        self.store(ctx, MemKind::I32, object, record::SHAPE, shape);
    }
}
