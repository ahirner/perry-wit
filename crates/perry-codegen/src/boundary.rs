//! Canonical ABI conversion for typed component imports and exports.
//!
//! Strings share UTF-8 payloads across the boundary, while adopted input buffers and borrowed
//! output owners remain live through lifting, post-return, or async acknowledgement. Results
//! support explicit tags and checked completion-style returns and throws. Async exports use
//! stackless P3 callbacks, and async imports retain a canonical transfer until the host settles it.
use crate::{
    aggregates::{RecordField, RecordLayout, RecordLayouts},
    function::FunctionLowering,
    host::{EffectHost, Foreign, ValueHost},
    module::ModuleState,
    repr::{Carriers, Repr, i32_type, repr, wasm_type},
    runtime::Runtime,
    values::is_numeric_type,
};
use perry_abi::layout::{array as layout, string as strings};
use perry_contract::types::{Integer, Scalar};
use perry_mir::{ArrayType, RecordType};
use perry_wit_diagnostics::{
    Result, bail_diag, bail_internal, bail_unsupported, codes, diag, ensure_diag, ensure_internal,
    internal,
};
use pliron::{
    builtin::{
        op_interfaces::{SingleBlockRegionInterface, SymbolOpInterface},
        ops::FuncOp,
        types::{FP32Type, FP64Type, FunctionType, IntegerType, Signedness},
    },
    context::Context,
    identifier::Identifier,
    op::Op,
    r#type::{TypeHandle, TypedHandle},
    value::Value,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    ops::{CallOp, set_export},
};
use std::sync::Arc;
use wit_parser::abi::{WasmSignature, WasmType};

mod canonical;
mod exports;
mod imports;
mod integers;
mod parameters;
mod resources;
mod scopes;
mod sums;

use canonical::Canonical;
pub(super) use exports::export;
pub(super) use resources::emit_drain;
pub(super) use scopes::Scope;
use sums::BoundaryValue;
pub use sums::SumLayout;

/// A WIT value type as the canonical ABI sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct AbiType {
    pub kind: AbiKind,
    pub flat: Option<Box<[WasmType]>>,
    pub size: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AbiField {
    pub ty: Arc<AbiType>,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AbiKind {
    Scalar(Scalar),
    String,
    List(Arc<AbiType>),
    /// Fields in WIT order, named as the TypeScript record fields.
    Record(AbiRecord),
    Sum(SumLayout),
    /// `stream<u8>`: the readable end's handle, a `ReadableStream<Uint8Array>` in TypeScript.
    ByteStream,
    Resource {
        key: u32,
        owned: bool,
    },
}

#[derive(Clone, Debug)]
pub struct AbiRecord {
    wit: wit_parser::Type,
    fields: Vec<(String, AbiField)>,
}

impl AbiKind {
    pub fn record(wit: wit_parser::Type, fields: impl Iterator<Item = (String, AbiField)>) -> Self {
        Self::Record(AbiRecord {
            wit,
            fields: fields.collect(),
        })
    }
}

// Equal structural signatures can share numeric constraints across named WIT types.
impl PartialEq for AbiRecord {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
    }
}

impl RecordLayouts {
    fn boundary_record(&self, ctx: &Context, mir_ty: TypeHandle) -> Result<&RecordLayout> {
        let Some(record) = RecordType::data_record(ctx, mir_ty) else {
            bail_diag!(
                codes::CONTRACT_MISMATCH,
                "WIT record needs a TypeScript object type"
            )
        };
        self.get(ctx, record)
    }
}

#[derive(Default)]
pub(crate) struct WitSlots {
    records: std::collections::HashMap<
        (TypedHandle<RecordType>, wit_parser::Type),
        std::ops::Range<usize>,
    >,
    fields: Vec<RecordField>,
}

impl WitSlots {
    fn bind(
        &mut self,
        ctx: &Context,
        layout: &RecordLayout,
        record: &AbiRecord,
    ) -> Result<std::ops::Range<usize>> {
        let span = match self.records.entry((layout.record, record.wit)) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let start = self.fields.len();
                for (name, _) in &record.fields {
                    match layout.field(ctx, name) {
                        Ok(field) => self.fields.push(field),
                        Err(error) => {
                            self.fields.truncate(start);
                            return Err(error);
                        }
                    }
                }
                entry.insert(start..self.fields.len())
            }
        };
        Ok(span.clone())
    }
}

/// A statically typed host import from the selected WIT world.
#[derive(Clone, Debug)]
pub struct ResourceDefinition {
    pub key: u32,
    pub module: String,
    pub name: String,
    pub exported: bool,
}
pub use perry_contract::ResourceOperation;
#[derive(Clone, Debug)]
pub enum ImportKind {
    Function(WasmSignature),
    Resource {
        key: u32,
        operation: ResourceOperation,
    },
}
#[derive(Clone, Debug)]
pub struct Import {
    pub kind: ImportKind,
    pub binding: String,
    pub module: String,
    pub name: String,
    pub params: Vec<AbiField>,
    pub indirect_size: Option<u32>,
    pub result: Option<Arc<AbiType>>,
    pub asynchronous: bool,
}

/// Synchronous vs. callback-lifted async export mode.
#[derive(Clone, Debug)]
pub enum ExportKind {
    Sync,
    Async {
        /// The import module and WIT function name of `task.return`.
        task_return: (String, String),
        completion: AsyncCompletion,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum AsyncCompletion {
    Function,
    Command,
    /// A rejected task supplies its diagnostic text to this nested WIT case.
    RejectionText(&'static [&'static str]),
}

/// One component export bound to a TypeScript function.
#[derive(Clone, Debug)]
pub struct Export {
    pub source_name: Option<String>,
    /// Parameter annotation spans followed by the return annotation.
    pub annotations: Vec<Option<perry_wit_diagnostics::SourceSpan>>,
    /// Core export name: the WIT function name, or `interface#function`.
    pub core_name: String,
    pub signature: WasmSignature,
    /// The MIR function implementing it.
    pub function: String,
    pub params: Vec<AbiField>,
    pub indirect_size: Option<u32>,
    pub result: Option<Arc<AbiType>>,
    pub kind: ExportKind,
}

impl Export {
    pub fn declaration(&self) -> &str {
        self.source_name.as_deref().unwrap_or(&self.function)
    }
    pub fn annotation(&self, i: usize) -> Option<perry_wit_diagnostics::SourceSpan> {
        self.annotations.get(i).cloned().flatten()
    }
    pub(crate) fn is_async(&self) -> bool {
        matches!(self.kind, ExportKind::Async { .. })
    }

    pub(crate) fn exception_result(&self) -> Option<&AbiType> {
        let result = self.result.as_deref()?;
        match self.kind {
            ExportKind::Async {
                completion: AsyncCompletion::RejectionText(_),
                ..
            } => Some(result),
            _ => result.accepts_exception().then_some(result),
        }
    }

    pub fn is_command(&self) -> bool {
        matches!(
            self.kind,
            ExportKind::Async {
                completion: AsyncCompletion::Command,
                ..
            }
        )
    }
}

impl AbiType {
    /// Whether this type is one of the 11 Canonical ABI scalars.
    pub(super) fn is_scalar(&self) -> bool {
        matches!(self.kind, AbiKind::Scalar(_))
    }

    fn direct(&self) -> Result<&[WasmType]> {
        self.flat
            .as_deref()
            .ok_or_else(|| internal!("indirect WIT type in a flat signature"))
    }
}

fn flat_type(ctx: &Context, ty: WasmType) -> TypeHandle {
    match ty {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => i32_type(ctx),
        WasmType::I64 | WasmType::PointerOrI64 => {
            IntegerType::get(ctx, 64, Signedness::Signless).into()
        }
        WasmType::F32 => FP32Type::get(ctx).into(),
        WasmType::F64 => FP64Type::get(ctx).into(),
    }
}

pub(crate) fn core_types(ctx: &Context, types: &[WasmType]) -> Vec<TypeHandle> {
    types.iter().map(|&ty| flat_type(ctx, ty)).collect()
}

/// Symbol of a canonical-ABI glue function named after a WIT item.
fn identifier(name: &str) -> Identifier {
    crate::host::symbol(pliron::ident!("wit_"), name)
}

fn list_element_type(ctx: &Context, ty: TypeHandle) -> Result<TypeHandle> {
    perry_mir::typed::<ArrayType>(ctx, ty)
        .map(|array| array.deref(ctx).element())
        .ok_or_else(|| {
            diag!(
                codes::CONTRACT_MISMATCH,
                "WIT list needs a TypeScript array"
            )
        })
}

fn expect_string(ctx: &Context, ty: TypeHandle) -> Result<()> {
    ensure_diag!(
        ty.deref(ctx).is::<perry_mir::StringType>(),
        codes::CONTRACT_MISMATCH,
        "WIT string needs a TypeScript string"
    );
    Ok(())
}

fn expect_number(ctx: &Context, mir_ty: TypeHandle, scalar: Scalar) -> Result<()> {
    ensure_diag!(
        mir_ty.deref(ctx).is::<FP64Type>(),
        codes::CONTRACT_MISMATCH,
        "WIT {} needs a TypeScript number",
        scalar
    );
    Ok(())
}

pub(super) fn scalar_mir_type(ctx: &mut Context, mir_ty: TypeHandle) -> TypeHandle {
    if mir_ty.deref(ctx).is::<perry_mir::NumericType>() {
        return FP64Type::get(ctx).into();
    }
    if let Some(optional) = perry_mir::typed::<perry_mir::OptionalType>(ctx, mir_ty) {
        let (value, absence) = {
            let opt = optional.deref(ctx);
            (opt.value(), opt.absence())
        };
        if value.deref(ctx).is::<perry_mir::NumericType>() {
            let f64_ty = FP64Type::get(ctx).into();
            return perry_mir::OptionalType::get_with(ctx, f64_ty, absence).into();
        }
    }
    mir_ty
}

fn scalar_memory(scalar: Scalar) -> MemKind {
    match scalar {
        Scalar::Bool | Scalar::Integer(Integer::U8) => MemKind::I32U8,
        Scalar::Integer(Integer::I8) => MemKind::I32S8,
        Scalar::Integer(Integer::U16) => MemKind::I32U16,
        Scalar::Integer(Integer::I16) => MemKind::I32S16,
        Scalar::Integer(Integer::I32 | Integer::U32) => MemKind::I32,
        Scalar::Integer(_) => MemKind::I64,
        Scalar::F32 => MemKind::F32,
        Scalar::F64 => MemKind::F64,
    }
}

impl FunctionLowering<'_, '_> {
    pub(super) fn close_web_handles(&mut self, ctx: &mut Context, include_serve: bool) {
        for (used, function) in [
            (
                include_serve && self.state.uses_http_server,
                Runtime::WebServeClose,
            ),
            (self.state.uses_fetch, Runtime::WebFetchClose),
            (self.state.uses_streams, Runtime::WebStreamsClose),
        ] {
            if used {
                self.runtime_void(ctx, function, vec![]);
            }
        }
    }

    pub(super) fn trap_unless(&mut self, ctx: &mut Context, cond: Value, free_task: bool) {
        let valid = self.new_block(ctx, vec![]);
        let trap = self.new_block(ctx, vec![]);
        self.cond_br(ctx, cond, (valid, vec![]), (trap, vec![]));
        self.block = trap;
        if free_task {
            self.task_free(ctx);
        }
        self.trap(ctx);
        self.block = valid;
    }

    /// Acknowledge every outstanding host transfer before releasing a cancelled task.
    fn abort_task(&mut self, ctx: &mut Context) -> Result<()> {
        self.runtime(ctx, Runtime::TaskCancel, vec![]);
        self.cleanup_foreign(ctx)?;
        if self.state.uses_fs {
            self.runtime(ctx, Runtime::FsClose, vec![]);
        }
        self.close_web_handles(ctx, true);
        if self.state.uses_io || self.state.reports_rejection {
            self.runtime_void(ctx, Runtime::IoCancel, vec![]);
        }
        self.runtime_void(ctx, Runtime::TaskCloseSet, vec![]);
        self.return_resources(ctx);
        self.task_free(ctx);
        if !self.state.resources.is_empty() {
            self.runtime_void(ctx, Runtime::GcBoundary, vec![]);
        }
        self.drain_resources(ctx);
        Ok(())
    }

    fn lift_scalar(
        &mut self,
        ctx: &mut Context,
        scalar: Scalar,
        mir: TypeHandle,
        raw: Value,
    ) -> Result<Value> {
        let opcode = match scalar {
            Scalar::Integer(domain) if matches!(repr(ctx, mir)?, Repr::I32 | Repr::I64) => {
                let source = perry_mir::integer_type(ctx, domain).into();
                let value = self.normalize_integer(ctx, raw, source);
                return self.numeric_cast(ctx, value, source, mir);
            }
            Scalar::Integer(domain) if domain.bits() == 64 => {
                return self.lift_integer(ctx, domain.signed(), mir, raw);
            }
            Scalar::Integer(domain) if domain.signed() => NumOpcode::F64ConvertI32S,
            Scalar::Integer(_) => NumOpcode::F64ConvertI32U,
            Scalar::Bool => {
                ensure_diag!(
                    repr(ctx, mir)? == Repr::Bool,
                    codes::CONTRACT_MISMATCH,
                    "WIT bool needs a TypeScript boolean"
                );
                let zero = self.i32_const(ctx, 0);
                return Ok(self.num(ctx, NumOpcode::I32Ne, vec![raw, zero]));
            }
            Scalar::F32 => NumOpcode::F64PromoteF32,
            Scalar::F64 => {
                expect_number(ctx, mir, scalar)?;
                return Ok(raw);
            }
        };
        expect_number(ctx, mir, scalar)?;
        Ok(self.num(ctx, opcode, vec![raw]))
    }

    fn lift_abi(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        mut source: Canonical<&mut &[Value]>,
    ) -> Result<Value> {
        match &ty.kind {
            AbiKind::String | AbiKind::List(_) => {
                let ptr = source.read(self, ctx, MemKind::I32, 0)?;
                let len = source.read(self, ctx, MemKind::I32, 4)?;
                let value = match &ty.kind {
                    AbiKind::List(element)
                        if element.kind == AbiKind::Scalar(Scalar::Integer(Integer::U8))
                            && mir_ty.deref(ctx).is::<perry_mir::BytesType>() =>
                    {
                        self.runtime(ctx, Runtime::StdBytesAdopt, vec![len, ptr])
                    }
                    AbiKind::List(element) => self.lift_list(ctx, element, mir_ty, ptr, len)?,
                    _ => {
                        expect_string(ctx, mir_ty)?;
                        self.runtime(ctx, Runtime::StringAdopt, vec![ptr, len])
                    }
                };
                if self.state.collectible {
                    self.runtime_void(ctx, Runtime::CabiRelease, vec![ptr]);
                }
                Ok(value)
            }
            AbiKind::ByteStream => {
                let handle = source.read(self, ctx, MemKind::I32, 0)?;
                self.lift_stream(ctx, mir_ty, handle)
            }
            AbiKind::Record(record) => {
                let layout = self.state.record_layouts.boundary_record(ctx, mir_ty)?;
                let slots = self.state.wit_slots.bind(ctx, layout, record)?;
                let mut values = Vec::with_capacity(slots.len());
                for ((_, abi), slot) in record.fields.iter().zip(slots) {
                    let field = self.state.wit_slots.fields[slot];
                    let mir = scalar_mir_type(ctx, field.ty);
                    values.push((
                        field,
                        self.lift_abi(ctx, &abi.ty, mir, source.at(abi.offset))?,
                    ));
                }
                self.build_record(ctx, layout, values)
            }
            AbiKind::Scalar(scalar) => {
                let raw = source.read(self, ctx, scalar_memory(*scalar), 0)?;
                self.lift_scalar(ctx, *scalar, mir_ty, raw)
            }
            AbiKind::Resource { key, owned } => {
                let raw = source.read(self, ctx, MemKind::I32, 0)?;
                self.lift_resource(ctx, *key, *owned, mir_ty, raw)
            }
            AbiKind::Sum(sum) => self.lift_sum(ctx, ty, sum, mir_ty, source),
        }
    }

    /// A `ReadableStream<Uint8Array>` owning the readable end `handle`.
    fn lift_stream(
        &mut self,
        ctx: &mut Context,
        mir_ty: TypeHandle,
        handle: Value,
    ) -> Result<Value> {
        ensure_diag!(
            mir_ty.deref(ctx).is::<perry_mir::ReadableType>(),
            codes::CONTRACT_MISMATCH,
            "WIT stream<u8> needs a TypeScript ReadableStream<Uint8Array>"
        );
        self.state.uses_streams = true;
        Ok(self.runtime(ctx, Runtime::WebReadableNew, vec![handle]))
    }

    fn list_elements(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        buffer: Value,
        len: Value,
        body: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<()>,
    ) -> Result<()> {
        self.counted_loop(ctx, len, |b, ctx, index| {
            let stride = b.i32_const(ctx, element.size as i32);
            let offset = b.num(ctx, NumOpcode::I32Mul, vec![index, stride]);
            let address = b.num(ctx, NumOpcode::I32Add, vec![buffer, offset]);
            body(b, ctx, index, address)
        })
    }

    fn lift_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        ptr: Value,
        len: Value,
    ) -> Result<Value> {
        let element_mir = list_element_type(ctx, mir_ty)?;
        let lift_mir = scalar_mir_type(ctx, element_mir);
        let array = self.new_array(ctx, len, element_mir)?;
        let data = self.load(ctx, MemKind::I32, array, layout::DATA);
        self.list_elements(ctx, element, ptr, len, |b, ctx, i, source| {
            let value = b.lift_abi(ctx, element, lift_mir, Canonical::Memory(source, 0))?;
            let carriers = b.cast_value(ctx, Carriers::Scalar(value), lift_mir, element_mir)?;
            let slot = b.slot_address(ctx, data, i, element_mir);
            b.store_slot_value(ctx, slot, 0, element_mir, carriers)?;
            Ok(())
        })?;
        self.store(ctx, MemKind::I32, array, layout::LEN, len);
        Ok(array)
    }

    /// Store fields in native slots and preserve their incoming insertion order.
    fn build_record(
        &mut self,
        ctx: &mut Context,
        layout: &RecordLayout,
        values: Vec<(RecordField, Value)>,
    ) -> Result<Value> {
        let field_count = layout.fields.len();
        ensure_diag!(
            values.len() == field_count,
            codes::CONTRACT_MISMATCH,
            "the WIT record has {} fields but the TypeScript type {} has {field_count}",
            values.len(),
            layout.record.deref(ctx).name()
        );
        let object = self.record_storage(ctx, layout)?;
        let order: Vec<_> = values.iter().map(|(field, _)| field.index).collect();
        let order = self.record_order(field_count, Some(&order));
        self.record_header(ctx, object, order);
        for (field, value) in values {
            let source = scalar_mir_type(ctx, field.ty);
            let carriers = self.cast_value(ctx, Carriers::Scalar(value), source, field.ty)?;
            self.store_slot_value(ctx, object, field.offset, field.ty, carriers)?;
        }
        Ok(object)
    }

    fn lower_scalar(
        &mut self,
        ctx: &mut Context,
        scalar: Scalar,
        mir: TypeHandle,
        value: Carriers<Value>,
    ) -> Result<Value> {
        match scalar {
            Scalar::Bool => value.scalar(),
            Scalar::Integer(domain) => {
                if matches!(repr(ctx, mir)?, Repr::I32 | Repr::I64) || is_numeric_type(ctx, mir) {
                    if domain.bits() == 64
                        && crate::numeric::integer(ctx, mir)
                            .is_some_and(|(bits, signed)| bits == 64 && signed != domain.signed())
                    {
                        self.check_integer_signedness(ctx, value.scalar()?);
                    }
                    let target = perry_mir::integer_type(ctx, domain).into();
                    return self.cast_value(ctx, value, mir, target)?.scalar();
                }
                let value = value.scalar()?;
                if domain.bits() == 64 {
                    return self.lower_integer(ctx, domain.signed(), mir, value);
                }
                expect_number(ctx, mir, scalar)?;
                Ok(self.int32_bits(ctx, value, domain.signed(), None))
            }
            Scalar::F32 | Scalar::F64 => {
                let number = if crate::numeric::integer(ctx, mir).is_some_and(|(bits, _)| bits > 1)
                    || is_numeric_type(ctx, mir)
                {
                    self.cast_value(ctx, value, mir, FP64Type::get(ctx).into())?
                        .scalar()?
                } else {
                    expect_number(ctx, mir, scalar)?;
                    value.scalar()?
                };
                Ok(if scalar == Scalar::F32 {
                    self.num(ctx, NumOpcode::F32DemoteF64, vec![number])
                } else {
                    number
                })
            }
        }
    }

    /// Copy a MIR array into a canonical list buffer and return the `(buffer, length)` pair.
    fn lower_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<(Value, Value)> {
        if element.kind == AbiKind::Scalar(Scalar::Integer(Integer::U8))
            && (mir_ty.deref(ctx).is::<perry_mir::BytesType>()
                || mir_ty.deref(ctx).is::<perry_mir::ByteInputType>())
        {
            let view = self.runtime(ctx, Runtime::StdBytesBoundary, vec![value]);
            let data = self.load(ctx, MemKind::I32, view, 4);
            let len = self.load(ctx, MemKind::I32, view, 0);
            if self.canonical_borrows {
                self.publish_roots(ctx, &[data]);
            } else if self.state.collectible {
                self.runtime(ctx, Runtime::GcPin, vec![data]);
            }
            return Ok((data, len));
        }
        let element_mir = list_element_type(ctx, mir_ty)?;
        let len = self.load(ctx, MemKind::I32, value, layout::LEN);
        let data = self.load(ctx, MemKind::I32, value, layout::DATA);
        let stride = self.i32_const(ctx, element.size as i32);
        let bytes = self.num(ctx, NumOpcode::I32Mul, vec![len, stride]);
        let buffer = self.alloc(ctx, bytes);
        self.list_elements(ctx, element, buffer, len, |b, ctx, i, target| {
            let slot = b.slot_address(ctx, data, i, element_mir);
            let item = b.load_slot_value(ctx, slot, 0, element_mir)?;
            let mut done = None;
            if b.state.program_facts.has_array_holes() {
                let present = b.array_slot_present(ctx, value, i);
                if matches!(element.kind, AbiKind::Sum(SumLayout::Option(_))) {
                    let existing = b.new_block(ctx, vec![]);
                    let missing = b.new_block(ctx, vec![]);
                    let next = b.new_block(ctx, vec![]);
                    b.cond_br(ctx, present, (existing, vec![]), (missing, vec![]));
                    b.block = missing;
                    let none = b.i32_const(ctx, 0);
                    b.store(ctx, MemKind::I32U8, target, 0, none);
                    b.br(ctx, next, vec![]);
                    b.block = existing;
                    done = Some(next);
                } else {
                    b.trap_unless(ctx, present, true);
                }
            }
            b.lower_abi(
                ctx,
                element,
                BoundaryValue::Native(element_mir, item),
                Canonical::Memory(target, 0),
            )?;
            if let Some(done) = done {
                b.br(ctx, done, vec![]);
                b.block = done;
            }
            Ok(())
        })?;
        Ok((buffer, len))
    }

    fn lower_abi(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        source: BoundaryValue,
        mut target: Canonical<&mut Vec<Value>>,
    ) -> Result<()> {
        if let AbiKind::Sum(sum) = &ty.kind {
            return self.lower_sum(ctx, ty, sum, source, target);
        }
        let BoundaryValue::Native(mir_ty, value) = source else {
            bail_internal!("WIT completion path enters a non-sum type");
        };
        match &ty.kind {
            AbiKind::ByteStream => {
                bail_unsupported!("returning a stream<u8> is not supported yet")
            }
            AbiKind::String | AbiKind::List(_) => {
                let value = value.scalar()?;
                let (data, len) = match &ty.kind {
                    AbiKind::List(element)
                        if element.kind != AbiKind::Scalar(Scalar::Integer(Integer::U8))
                            || !mir_ty.deref(ctx).is::<perry_mir::StringType>() =>
                    {
                        self.lower_list(ctx, element, mir_ty, value)?
                    }
                    _ => {
                        expect_string(ctx, mir_ty)?;
                        (
                            self.load(ctx, MemKind::I32, value, strings::DATA),
                            self.load(ctx, MemKind::I32, value, strings::LEN),
                        )
                    }
                };
                target.write(self, ctx, MemKind::I32, 0, data);
                target.write(self, ctx, MemKind::I32, 4, len);
            }
            AbiKind::Record(record) => {
                let value = value.scalar()?;
                let layout = self.state.record_layouts.boundary_record(ctx, mir_ty)?;
                let slots = self.state.wit_slots.bind(ctx, layout, record)?;
                for ((_, field_ty), slot) in record.fields.iter().zip(slots) {
                    let field = self.state.wit_slots.fields[slot];
                    let value = self.load_slot_value(ctx, value, field.offset, field.ty)?;
                    self.lower_abi(
                        ctx,
                        &field_ty.ty,
                        BoundaryValue::Native(field.ty, value),
                        target.at(field_ty.offset),
                    )?;
                }
            }
            AbiKind::Scalar(scalar) => {
                let raw = self.lower_scalar(ctx, *scalar, mir_ty, value)?;
                target.write(self, ctx, scalar_memory(*scalar), 0, raw);
            }
            AbiKind::Resource { key, owned } => {
                let raw = self.lower_resource(ctx, *key, *owned, mir_ty, value.scalar()?)?;
                target.write(self, ctx, MemKind::I32, 0, raw);
            }
            _ => bail_internal!("expected a non-sum WIT type"),
        }
        Ok(())
    }
}
