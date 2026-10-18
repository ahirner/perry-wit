//! Records, arrays and variants: allocation, field and element access.
use crate::{
    function::FunctionLowering,
    repr::{Carriers, Repr, optional_inner, repr, slot_kind, slot_size},
    runtime::Runtime,
    values::is_numeric_type,
};
use anyhow::{Result, anyhow};
use perry_abi::layout::{array as layout, record, variant};
use perry_mir::{
    AllocOp, ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
    ArrayTruncateOp, ArrayType, ConcatSegmentsOp, ErrorKind, FieldGetOp, FieldPresentOp,
    FieldSetOp, RecordType, VariantNewOp, VariantPayloadOp, VariantTagOp,
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
    /// The runtime's element-kind flag for arrays of `element` (`layout::*_ELEMENTS`).
    pub(crate) fn array_element_kind(
        ctx: &Context,
        element: TypeHandle,
    ) -> Result<layout::Elements> {
        if is_numeric_type(ctx, element) {
            Ok(layout::Elements::Numeric)
        } else if repr(ctx, element)? == Repr::Ref {
            Ok(layout::Elements::References)
        } else {
            Ok(layout::Elements::Scalar)
        }
    }

    /// Byte size of `record`: its header and every field slot.
    pub(crate) fn record_size(ctx: &Context, record: TypedHandle<RecordType>) -> Result<u32> {
        let record = record.deref(ctx);
        let fields = record
            .fields()
            .map_err(|_| anyhow!("record without fields"))?;
        Ok(record_offset(ctx, fields))
    }

    /// Store `value` into a `ty` slot at `addr + offset`.
    pub(crate) fn store_slot_value(
        &mut self,
        ctx: &mut Context,
        addr: Value,
        offset: u32,
        ty: TypeHandle,
        value: Carriers<Value>,
    ) -> Result<()> {
        match value {
            Carriers::Scalar(v) => {
                let kind = slot_kind(ctx, ty)?;
                self.store(ctx, kind, addr, offset, v);
            }
            Carriers::Numeric { tag, payload } => {
                self.store(ctx, MemKind::I32, addr, offset, tag);
                self.store(ctx, MemKind::I64, addr, offset + 8, payload);
            }
        }
        Ok(())
    }

    /// Load the carriers of a `ty` slot at `addr + offset`.
    pub(crate) fn load_slot_value(
        &mut self,
        ctx: &mut Context,
        addr: Value,
        offset: u32,
        ty: TypeHandle,
    ) -> Result<Carriers<Value>> {
        if is_numeric_type(ctx, ty) {
            let tag = self.load(ctx, MemKind::I32, addr, offset);
            let payload = self.load(ctx, MemKind::I64, addr, offset + 8);
            Ok(Carriers::Numeric { tag, payload })
        } else {
            let kind = slot_kind(ctx, ty)?;
            let raw = self.load(ctx, kind, addr, offset);
            Ok(Carriers::Scalar(raw))
        }
    }

    pub(crate) fn array_concat(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        let element = Self::element_type(ctx, op.deref(ctx).get_result(0))?;
        let operands = self.operands(ctx, op)?;
        let references = self.i32_const(ctx, Self::array_element_kind(ctx, element)? as i32);
        let reuse = Operation::get_op::<perry_mir::ConcatReuseOp>(op, ctx)
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
        if let Some(allocation) = Operation::get_op::<AllocOp>(op, ctx) {
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
            let byte_size = Self::record_size(ctx, record)?;
            let size = self.i32_const(ctx, byte_size as i32);
            let object = if let Some(&offset) = self.frame_slots.get(&op) {
                let base = self
                    .local_frame
                    .ok_or_else(|| anyhow!("frame-resident record without frame storage"))?;
                let offset = self.i32_const(ctx, offset as i32);
                self.num(ctx, NumOpcode::I32Add, vec![base, offset])
            } else {
                self.record_storage(ctx, record_ty, size)?
            };
            let order = {
                let order = allocation.get_attr_perry_field_order(ctx);
                self.record_order(ctx, record, order.as_ref().map(|order| order.0.as_slice()))?
            };
            self.record_header(ctx, object, order);
            let mut offset = record::HEADER;
            for (index, ty) in fields.into_iter().enumerate() {
                let operand = op.deref(ctx).get_operand(index);
                let value = self.mapped_value(ctx, operand)?;
                self.store_slot_value(ctx, object, offset, ty, value)?;
                offset += slot_size(ctx, ty);
            }
            self.define(ctx, op, object);
            return Ok(true);
        }
        if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let (offset, ty) = Self::field_slot(ctx, record, &get.field(ctx))?;
            let object = self.mapped(ctx, record)?;
            let mut value = self.load_slot_value(ctx, object, offset, ty)?;
            if perry_mir::absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
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
            let value = if perry_mir::absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
                let object = self.mapped(ctx, record)?;
                let unpacked = self.load_slot_value(ctx, object, offset, ty)?;
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
            let object = self.mapped(ctx, record)?;
            let value = self.mapped_value(ctx, val_opd)?;
            if perry_mir::absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
                let (table, count) = self.json_field_table(ctx, record.get_type(ctx))?;
                let table = self.i32_const(ctx, table as i32);
                let count = self.i32_const(ctx, count as i32);
                let key = self.state.data.string(&set.field(ctx));
                let key = self.i32_const(ctx, key as i32);
                let omitted = self.is_omitted_value(ctx, value, ty)?;
                let present = self.num(ctx, NumOpcode::I32Eqz, vec![omitted]);
                self.runtime_void(
                    ctx,
                    Runtime::StdJsonRecordField,
                    vec![object, table, count, key, present],
                );
            }
            self.store_slot_value(ctx, object, offset, ty, value)?;
            return Ok(true);
        }
        if Operation::get_op::<ArrayNewOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_result(0))?;
            let count = op.deref(ctx).get_num_operands();
            let capacity = self.i32_const(ctx, count as i32);
            let array = self.new_array(ctx, capacity, element)?;
            if count > 0 {
                let mut slots = Vec::with_capacity(count);
                for index in 0..count {
                    let operand = op.deref(ctx).get_operand(index);
                    slots.push(self.mapped_value(ctx, operand)?);
                }
                let data = self.load(ctx, MemKind::I32, array, layout::DATA);
                let stride = slot_size(ctx, element);
                for (index, slot) in slots.into_iter().enumerate() {
                    self.store_slot_value(ctx, data, stride * index as u32, element, slot)?;
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
        if let Some(native) = Operation::get_op::<perry_mir::NativeJsonOp>(op, ctx) {
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
            let references = self.i32_const(ctx, Self::array_element_kind(ctx, element)? as i32);
            let value = self.runtime(ctx, Runtime::ArrayConcatSegments, vec![sources, references]);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ArrayExtendOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let operands = self.operands(ctx, op)?;
            let references = self.i32_const(ctx, Self::array_element_kind(ctx, element)? as i32);
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
            let array = self.mapped(ctx, arr_opd)?;
            let value = self.mapped_value(ctx, val_opd)?;
            self.push(ctx, array, value, element)?;
            return Ok(true);
        }
        if Operation::get_op::<ArrayGetOp>(op, ctx).is_some() {
            // Branch-free: read slot 0 when out of range (capacity is never 0), then select.
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let operands = self.operands(ctx, op)?;
            let (array, index) = (operands[0], operands[1]);
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, _) = self.element_index(ctx, index, len)?;
            let zero = self.i32_const(ctx, 0);
            let slot = self.select(ctx, ok, i, zero);
            let data = self.load(ctx, MemKind::I32, array, layout::DATA);
            let address = self.slot_address(ctx, data, slot, element);
            let loaded = self.load_slot_value(ctx, address, 0, element)?;
            let target = op.deref(ctx).get_result(0).get_type(ctx);
            if optional_inner(ctx, element).is_some() {
                self.check_optional_cast(ctx, loaded, element, target)?;
            }
            let undefined = self.undefined_value(ctx, target)?;
            let value = self.select_carriers(ctx, ok, loaded, undefined)?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ArrayAtOp>(op, ctx).is_some() {
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let operands = self.operands(ctx, op)?;
            let data = self.load(ctx, MemKind::I32, operands[0], layout::DATA);
            let address = self.slot_address(ctx, data, operands[1], element);
            let value = self.load_slot_value(ctx, address, 0, element)?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
            let arr_opd = op.deref(ctx).get_operand(0);
            let idx_opd = op.deref(ctx).get_operand(1);
            let val_opd = op.deref(ctx).get_operand(2);
            let element = Self::element_type(ctx, arr_opd)?;
            let array = self.mapped(ctx, arr_opd)?;
            let index = self.mapped(ctx, idx_opd)?;
            let value = self.mapped_value(ctx, val_opd)?;
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, integral) = self.element_index(ctx, index, len)?;
            let write = self.new_block(ctx, vec![]);
            let outside = self.new_block(ctx, vec![]);
            let done = self.new_block(ctx, vec![]);
            self.cond_br(ctx, ok, (write, vec![]), (outside, vec![]));
            self.block = write;
            let data = self.load(ctx, MemKind::I32, array, layout::DATA);
            let address = self.slot_address(ctx, data, i, element);
            self.store_slot_value(ctx, address, 0, element, value)?;
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
            self.push(ctx, array, value, element)?;
            self.br(ctx, done, vec![]);
            self.block = done;
            return Ok(true);
        }
        Ok(false)
    }

    fn select_carriers(
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

    /// Address of element `index` in array storage `data` holding `element` slots.
    pub(crate) fn slot_address(
        &mut self,
        ctx: &mut Context,
        data: Value,
        index: Value,
        element: TypeHandle,
    ) -> Value {
        let shift = self.i32_const(ctx, slot_size(ctx, element).trailing_zeros() as i32);
        let offset = self.num(ctx, NumOpcode::I32Shl, vec![index, shift]);
        self.num(ctx, NumOpcode::I32Add, vec![data, offset])
    }

    /// Append `value` to `array`, growing it through the runtime when full.
    fn push(
        &mut self,
        ctx: &mut Context,
        array: Value,
        value: Carriers<Value>,
        element: TypeHandle,
    ) -> Result<()> {
        let len = self.load(ctx, MemKind::I32, array, layout::LEN);
        let cap = self.load(ctx, MemKind::I32, array, layout::CAP);
        let full = self.num(ctx, NumOpcode::I32Eq, vec![len, cap]);
        let grow = self.new_block(ctx, vec![]);
        let write = self.new_block(ctx, vec![]);
        self.cond_br(ctx, full, (grow, vec![]), (write, vec![]));
        self.block = grow;
        let elem_kind = Self::array_element_kind(ctx, element)?;
        if self.state.collectible {
            let references = self.i32_const(ctx, elem_kind as i32);
            self.runtime_void(ctx, Runtime::ArrayGrowCollectible, vec![array, references]);
        } else if elem_kind == layout::Elements::Numeric {
            let references = self.i32_const(ctx, elem_kind as i32);
            self.runtime_void(ctx, Runtime::ArrayGrowTyped, vec![array, references]);
        } else {
            self.runtime_void(ctx, Runtime::ArrayGrow, vec![array]);
        }
        self.br(ctx, write, vec![]);
        self.block = write;
        let data = self.load(ctx, MemKind::I32, array, layout::DATA);
        let address = self.slot_address(ctx, data, len, element);
        self.store_slot_value(ctx, address, 0, element, value)?;
        let one = self.i32_const(ctx, 1);
        let next = self.num(ctx, NumOpcode::I32Add, vec![len, one]);
        self.store(ctx, MemKind::I32, array, layout::LEN, next);
        Ok(())
    }

    pub(crate) fn element_type(ctx: &Context, array: Value) -> Result<TypeHandle> {
        let ty = TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected an array"))?;
        Ok(ty.deref(ctx).element())
    }

    pub(crate) fn record_type_field_slot(
        ctx: &Context,
        record: TypedHandle<RecordType>,
        field: &str,
    ) -> Result<(u32, TypeHandle)> {
        let record = record.deref(ctx);
        let fields = record
            .fields()
            .map_err(|_| anyhow!("record has no field {field}"))?;
        let (index, (_, field_ty)) = fields
            .iter()
            .enumerate()
            .find(|(_, (name, _))| name == field)
            .ok_or_else(|| anyhow!("TypeScript type {} has no field {field}", record.name()))?;
        Ok((record_offset(ctx, &fields[..index]), *field_ty))
    }

    fn field_slot(ctx: &Context, record: Value, field: &str) -> Result<(u32, TypeHandle)> {
        let record = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected a record"))?;
        Self::record_type_field_slot(ctx, record, field)
    }

    /// Encode static insertion order before emitting operations into the context.
    pub(crate) fn record_order(
        &mut self,
        ctx: &Context,
        record: TypedHandle<RecordType>,
        order: Option<&[u32]>,
    ) -> Result<u32> {
        let count = record.deref(ctx).fields()?.len();
        let length = order.map_or(count, <[_]>::len);
        let fields = (0..length).map(|index| order.map_or(index as u32, |order| order[index]));
        Ok(if count <= perry_abi::json::PACKED_FIELDS {
            fields.fold(0, perry_abi::json::append_field)
        } else {
            let mut bytes = Vec::with_capacity(4 * (length + 1));
            bytes.extend_from_slice(&(length as u32).to_le_bytes());
            for field in fields {
                bytes.extend_from_slice(&field.to_le_bytes());
            }
            self.state.data.append(4, &bytes)
        })
    }

    /// Initialize insertion order and, when needed, the retained JSON graph.
    pub(crate) fn record_header(&mut self, ctx: &mut Context, object: Value, order: u32) {
        if self.state.retains_json {
            let zero = self.i32_const(ctx, 0);
            self.store(ctx, MemKind::I32, object, record::JSON, zero);
        }
        let order = self.i32_const(ctx, order as i32);
        self.store(ctx, MemKind::I32, object, record::ORDER, order);
    }
}

/// Offset of the field following `preceding`: the header plus their slots.
fn record_offset(ctx: &Context, preceding: &[(String, TypeHandle)]) -> u32 {
    record::HEADER
        + preceding
            .iter()
            .map(|(_, ty)| slot_size(ctx, *ty))
            .sum::<u32>()
}
