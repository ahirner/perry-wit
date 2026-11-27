//! Records, arrays and variants: allocation, field and element access.
use crate::{
    function::FunctionLowering,
    repr::{Carriers, Repr, optional_inner, repr, slot_kind, slot_size},
    runtime::Runtime,
    values::is_numeric_type,
};
use indexmap::IndexMap;
use perry_abi::layout::{array as layout, record, variant};
use perry_mir::{
    AllocOp, ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayHoleOp, ArrayLenOp, ArrayNewOp,
    ArrayPresentOp, ArrayPushOp, ArraySetOp, ArrayStoreOp, ArrayTruncateOp, ArrayType,
    ConcatSegmentsOp, ErrorKind, FieldGetOp, FieldPresentOp, FieldSetOp, JsonSchema,
    PlironResultExt as _, RecordOrderOp, RecordType, VariantNewOp, VariantPayloadOp, VariantTagOp,
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag, ensure_unsupported};
use pliron::{
    builtin::types::FP64Type,
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};
use std::{
    cell::OnceCell,
    collections::{HashMap, hash_map::Entry},
    rc::Rc,
};

#[derive(Clone, Copy)]
pub(crate) struct RecordField {
    pub(crate) index: u32,
    pub(crate) offset: u32,
    pub(crate) ty: TypeHandle,
}

pub(crate) struct RecordLayout {
    pub(crate) record: TypedHandle<RecordType>,
    pub(crate) fields: IndexMap<String, RecordField>,
    pub(crate) size: u32,
    pointer_shapes: [OnceCell<u32>; 2],
    json_names: OnceCell<u32>,
}

impl RecordLayout {
    pub(crate) fn pointer_shape(
        &self,
        ctx: &Context,
        data: &mut crate::module::Data,
        retains_json: bool,
    ) -> u32 {
        *self.pointer_shapes[usize::from(retains_json)].get_or_init(|| {
            let offsets: Vec<_> = self
                .fields
                .values()
                .filter(|field| repr(ctx, field.ty).is_ok_and(|kind| kind == Repr::Ref))
                .map(|field| field.offset)
                .chain(retains_json.then_some(record::JSON))
                .collect();
            data.pointer_shape(&offsets)
        })
    }

    pub(super) fn json_fields(&self, data: &mut crate::module::Data) -> (u32, usize) {
        let names = self
            .json_names
            .get_or_init(|| data.field_names(self.fields.keys().map(String::as_str)));
        (*names, self.fields.len())
    }

    pub(crate) fn field(&self, ctx: &Context, name: &str) -> Result<RecordField> {
        self.fields.get(name).copied().ok_or_else(|| {
            diag!(
                codes::CONTRACT_MISMATCH,
                "TypeScript type {} has no field {name}",
                self.record.deref(ctx).name()
            )
        })
    }
}

#[derive(Default)]
pub(crate) struct RecordLayouts {
    native: HashMap<TypedHandle<RecordType>, Rc<RecordLayout>>,
    pub(super) wit_slots: HashMap<(TypedHandle<RecordType>, wit_parser::Type), Rc<[RecordField]>>,
}

impl RecordLayouts {
    pub(crate) fn get(
        &mut self,
        ctx: &Context,
        record: TypedHandle<RecordType>,
    ) -> Result<Rc<RecordLayout>> {
        let entry = match self.native.entry(record) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let mut size = record::HEADER;
                let mut fields = IndexMap::new();
                for (index, (name, ty)) in
                    record.deref(ctx).fields().to_diag(ctx)?.iter().enumerate()
                {
                    fields.insert(
                        name.clone(),
                        RecordField {
                            index: index as u32,
                            offset: size,
                            ty: *ty,
                        },
                    );
                    size += slot_size(ctx, *ty);
                }
                entry.insert(Rc::new(RecordLayout {
                    record,
                    fields,
                    size,
                    pointer_shapes: Default::default(),
                    json_names: OnceCell::new(),
                }))
            }
        };
        Ok(Rc::clone(entry))
    }
}

/// The byte holding an element's hole bit, and the bit's mask within it.
struct HoleBit {
    address: Value,
    byte: Value,
    mask: Value,
}

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
            let address = if reuse.is_empty() {
                0
            } else {
                self.state.data.words(reuse.iter().copied())
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
    /// Lower a record, array, or variant operation, returning `false` for other operations.
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
                .map_err(|_| Diagnostic::internal("expected a record"))?;
            let layout = self.state.record_layouts.get(ctx, record)?;
            let object = if let Some(&offset) = self.frame_slots.get(&op) {
                let base = self.local_frame.ok_or_else(|| {
                    Diagnostic::internal("frame-resident record without frame storage")
                })?;
                let offset = self.i32_const(ctx, offset as i32);
                self.num(ctx, NumOpcode::I32Add, vec![base, offset])
            } else {
                self.record_storage(ctx, &layout)
            };
            let order = {
                let order = allocation.get_attr_perry_field_order(ctx);
                self.record_order(
                    layout.fields.len(),
                    order.as_ref().map(|order| order.0.as_slice()),
                )
            };
            self.record_header(ctx, object, order);
            for field in layout.fields.values() {
                let operand = op.deref(ctx).get_operand(field.index as usize);
                let value = self.mapped_value(ctx, operand)?;
                self.store_slot_value(ctx, object, field.offset, field.ty, value)?;
            }
            self.define(ctx, op, object);
            return Ok(true);
        }
        if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let RecordField { offset, ty, .. } = self.field_slot(ctx, record, &get.field(ctx))?;
            let object = self.mapped(ctx, record)?;
            let mut value = self.load_slot_value(ctx, object, offset, ty)?;
            if perry_mir::absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
                let omitted = self.is_omitted(ctx, value, ty)?;
                let undef = self.undefined_value(ctx, ty)?;
                value = self.select_carriers(ctx, omitted, undef, value)?;
            }
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if let Some(present) = Operation::get_op::<FieldPresentOp>(op, ctx) {
            let record = op.deref(ctx).get_operand(0);
            let RecordField { offset, ty, .. } =
                self.field_slot(ctx, record, &present.field(ctx))?;
            let value = if perry_mir::absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
                let object = self.mapped(ctx, record)?;
                let unpacked = self.load_slot_value(ctx, object, offset, ty)?;
                let omitted = self.is_omitted(ctx, unpacked, ty)?;
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
            let RecordField { offset, ty, .. } = self.field_slot(ctx, record, &set.field(ctx))?;
            let object = self.mapped(ctx, record)?;
            let value = self.mapped_value(ctx, val_opd)?;
            if set.updates_order(ctx) {
                let (table, count) = self.json_field_table(ctx, record.get_type(ctx))?;
                let table = self.i32_const(ctx, table as i32);
                let count = self.i32_const(ctx, count as i32);
                let key = self.state.data.string(&set.field(ctx));
                let key = self.i32_const(ctx, key as i32);
                let present = if perry_mir::admits_undefined(ctx, ty) {
                    let omitted = self.is_omitted(ctx, value, ty)?;
                    self.num(ctx, NumOpcode::I32Eqz, vec![omitted])
                } else {
                    self.i32_const(ctx, 1)
                };
                self.runtime_void(
                    ctx,
                    Runtime::StdJsonRecordField,
                    vec![object, table, count, key, present],
                );
            }
            self.store_slot_value(ctx, object, offset, ty, value)?;
            return Ok(true);
        }
        if Operation::get_op::<RecordOrderOp>(op, ctx).is_some() {
            let record = op.deref(ctx).get_operand(0);
            let projection = op.deref(ctx).get_operand(1);
            let (table, count) = self.json_field_table(ctx, record.get_type(ctx))?;
            if count > 0 {
                let element = Self::element_type(ctx, projection)?;
                let record = self.mapped(ctx, record)?;
                let projection = self.mapped(ctx, projection)?;
                let table = self.i32_const(ctx, table as i32);
                let count = self.i32_const(ctx, count as i32);
                let stride = self.i32_const(ctx, slot_size(ctx, element) as i32);
                let retained = self.i32_const(ctx, i32::from(self.state.retains_json));
                self.runtime_void(
                    ctx,
                    Runtime::StdRecordOrder,
                    vec![record, projection, table, count, stride, retained],
                );
            }
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
        if Operation::is_op::<perry_mir::ArrayIsDenseOp>(op, ctx) {
            let operand = op.deref(ctx).get_operand(0);
            let dense = if self.state.program_facts.array_is_dense(ctx, operand) {
                self.i32_const(ctx, 1)
            } else {
                let array = self.mapped(ctx, operand)?;
                let holes = self.load(ctx, MemKind::I32, array, layout::HOLES);
                self.num(ctx, NumOpcode::I32Eqz, vec![holes])
            };
            self.define(ctx, op, dense);
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
            ensure_unsupported!(
                JsonSchema::supports_reflection(ctx, source.get_type(ctx)),
                "unsupported native cause field type"
            );
            let descriptor = self.layout_descriptor(ctx, source.get_type(ctx))?;
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
            let sparse = !self.state.program_facts.array_is_dense(ctx, arr_opd);
            self.push(ctx, array, value, element, sparse)?;
            return Ok(true);
        }
        if Operation::is_op::<ArrayHoleOp>(op, ctx) {
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let operands = self.operands(ctx, op)?;
            let index = if operands[1].get_type(ctx).deref(ctx).is::<FP64Type>() {
                self.num(ctx, NumOpcode::I32TruncSatF64U, vec![operands[1]])
            } else {
                operands[1]
            };
            let kind = self.i32_const(ctx, Self::array_element_kind(ctx, element)? as i32);
            self.runtime_void(ctx, Runtime::ArrayHole, vec![operands[0], index, kind]);
            return Ok(true);
        }
        if perry_mir::is_any::<(ArrayGetOp, ArrayPresentOp)>(ctx, op) {
            // Branch-free: read slot 0 when out of range (capacity is never 0), then select.
            let element = Self::element_type(ctx, op.deref(ctx).get_operand(0))?;
            let operands = self.operands(ctx, op)?;
            let (array, index) = (operands[0], operands[1]);
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, _) = self.element_index(ctx, index, len)?;
            let zero = self.i32_const(ctx, 0);
            let slot = self.select(ctx, ok, i, zero);
            let ok = if !self
                .state
                .program_facts
                .array_is_dense(ctx, op.deref(ctx).get_operand(0))
            {
                let present = self.array_slot_present(ctx, array, slot);
                self.num(ctx, NumOpcode::I32And, vec![ok, present])
            } else {
                ok
            };
            if Operation::is_op::<ArrayPresentOp>(op, ctx) {
                self.define(ctx, op, ok);
                return Ok(true);
            }
            let address = self.element_address(ctx, array, slot, element);
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
            let address = self.element_address(ctx, operands[0], operands[1], element);
            let value = self.load_slot_value(ctx, address, 0, element)?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        let store = Operation::get_op::<ArrayStoreOp>(op, ctx).is_some();
        if store || Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
            let arr_opd = op.deref(ctx).get_operand(0);
            let idx_opd = op.deref(ctx).get_operand(1);
            let val_opd = op.deref(ctx).get_operand(2);
            let element = Self::element_type(ctx, arr_opd)?;
            let array = self.mapped(ctx, arr_opd)?;
            let index = self.mapped(ctx, idx_opd)?;
            let value = self.mapped_value(ctx, val_opd)?;
            let sparse = !self.state.program_facts.array_is_dense(ctx, arr_opd);
            if store {
                self.store_element(ctx, array, index, element, value, sparse)?;
                return Ok(true);
            }
            let len = self.load(ctx, MemKind::I32, array, layout::LEN);
            let (i, ok, integral) = self.element_index(ctx, index, len)?;
            let write = self.new_block(ctx, vec![]);
            let outside = self.new_block(ctx, vec![]);
            let done = self.new_block(ctx, vec![]);
            self.cond_br(ctx, ok, (write, vec![]), (outside, vec![]));
            self.block = write;
            self.store_element(ctx, array, i, element, value, sparse)?;
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
            self.push(ctx, array, value, element, sparse)?;
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

    /// Whether slot `index` of `array` holds an element rather than a hole.
    pub(crate) fn array_slot_present(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
    ) -> Value {
        let present = self.i32_const(ctx, 1);
        self.hole_byte(ctx, array, index, Some(present), |b, ctx, bit| {
            let hole = b.num(ctx, NumOpcode::I32And, vec![bit.byte, bit.mask]);
            Some(b.num(ctx, NumOpcode::I32Eqz, vec![hole]))
        })
        .unwrap_or(present)
    }

    /// Run `sparse` on the hole bit of `index` when `array` has a hole bitset
    /// ([layout::HOLES]). The join receives `dense` without one, and otherwise what `sparse`
    /// returns.
    fn hole_byte(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
        dense: Option<Value>,
        sparse: impl FnOnce(&mut Self, &mut Context, HoleBit) -> Option<Value>,
    ) -> Option<Value> {
        let holes = self.load(ctx, MemKind::I32, array, layout::HOLES);
        let none = self.num(ctx, NumOpcode::I32Eqz, vec![holes]);
        let storage = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, dense.iter().map(|v| v.get_type(ctx)).collect());
        self.cond_br(
            ctx,
            none,
            (done, dense.into_iter().collect()),
            (storage, vec![]),
        );
        self.block = storage;
        let three = self.i32_const(ctx, 3);
        let offset = self.num(ctx, NumOpcode::I32ShrU, vec![index, three]);
        let address = self.num(ctx, NumOpcode::I32Add, vec![holes, offset]);
        let byte = self.load(ctx, MemKind::I32U8, address, 0);
        let seven = self.i32_const(ctx, 7);
        let shift = self.num(ctx, NumOpcode::I32And, vec![index, seven]);
        let one = self.i32_const(ctx, 1);
        let mask = self.num(ctx, NumOpcode::I32Shl, vec![one, shift]);
        let result = sparse(
            self,
            ctx,
            HoleBit {
                address,
                byte,
                mask,
            },
        );
        self.br(ctx, done, result.into_iter().collect());
        self.block = done;
        dense.map(|_| done.deref(ctx).get_argument(0))
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
        sparse: bool,
    ) -> Result<()> {
        let len = self.load(ctx, MemKind::I32, array, layout::LEN);
        let cap = self.load(ctx, MemKind::I32, array, layout::CAP);
        let full = self.num(ctx, NumOpcode::I32Eq, vec![len, cap]);
        let write = self.enter_if(ctx, full);
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
        self.store_element(ctx, array, len, element, value, sparse)?;
        let one = self.i32_const(ctx, 1);
        let next = self.num(ctx, NumOpcode::I32Add, vec![len, one]);
        self.store(ctx, MemKind::I32, array, layout::LEN, next);
        Ok(())
    }

    /// Address of element `index` of `array`, whose storage holds `element` slots.
    fn element_address(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
        element: TypeHandle,
    ) -> Value {
        let data = self.load(ctx, MemKind::I32, array, layout::DATA);
        self.slot_address(ctx, data, index, element)
    }

    fn store_element(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
        element: TypeHandle,
        value: Carriers<Value>,
        sparse: bool,
    ) -> Result<()> {
        let address = self.element_address(ctx, array, index, element);
        self.store_slot_value(ctx, address, 0, element, value)?;
        if sparse {
            self.hole_byte(ctx, array, index, None, |b, ctx, bit| {
                let all = b.i32_const(ctx, -1);
                let keep = b.num(ctx, NumOpcode::I32Xor, vec![bit.mask, all]);
                let present = b.num(ctx, NumOpcode::I32And, vec![bit.byte, keep]);
                b.store(ctx, MemKind::I32U8, bit.address, 0, present);
                None
            });
        }
        Ok(())
    }

    pub(crate) fn element_type(ctx: &Context, array: Value) -> Result<TypeHandle> {
        let ty = TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
            .map_err(|_| Diagnostic::internal("expected an array"))?;
        Ok(ty.deref(ctx).element())
    }

    fn field_slot(&mut self, ctx: &Context, record: Value, field: &str) -> Result<RecordField> {
        let record = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| Diagnostic::internal("expected a record"))?;
        self.state
            .record_layouts
            .get(ctx, record)?
            .field(ctx, field)
    }

    /// Encode static insertion order before emitting operations into the context.
    pub(crate) fn record_order(&mut self, count: usize, order: Option<&[u32]>) -> u32 {
        let length = order.map_or(count, <[_]>::len);
        let fields = (0..length).map(|index| order.map_or(index as u32, |order| order[index]));
        if count <= perry_abi::json::PACKED_FIELDS {
            fields.fold(0, perry_abi::json::append_field)
        } else {
            self.state
                .data
                .words(std::iter::once(length as u32).chain(fields))
        }
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
