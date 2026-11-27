//! Canonical ABI conversion for typed component imports and exports.
//!
//! Strings share UTF-8 payloads across the boundary, while adopted input buffers and borrowed
//! output owners remain live through lifting, post-return, or async acknowledgement. Results
//! support explicit tags and checked completion-style returns and throws. Async exports use
//! stackless P3 callbacks, and async imports retain a canonical transfer until the host settles it.
use crate::{
    aggregates::{RecordField, RecordLayout},
    function::FunctionLowering,
    host::{EffectHost, Foreign, ValueHost},
    module::ModuleState,
    repr::{Carriers, Repr, i32_type, repr, wasm_type},
    runtime::Runtime,
    values::is_numeric_type,
};
use perry_abi::layout::{array as layout, string as strings};
use perry_mir::{ArrayType, RecordType};
use perry_wit_diagnostics::{
    Diagnostic, Result, bail_diag, bail_internal, bail_unsupported, codes, diag, ensure_diag,
    ensure_internal, internal,
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
use wit_parser::{
    Type as Wit,
    abi::{WasmSignature, WasmType},
};

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
pub(super) use resources::{emit_drain, used_keys};
pub(super) use scopes::Scope;
use sums::BoundaryValue;

/// A WIT value type as the canonical ABI sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct AbiType {
    pub kind: AbiKind,
    pub flat: Option<Box<[Flat]>>,
    pub size: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AbiKind {
    Scalar(Wit),
    String,
    List(Box<AbiType>),
    /// Fields in WIT order, named as the TypeScript record fields.
    Record(Vec<(String, AbiType)>),
    Option(Box<AbiType>),
    Result {
        ok: Option<Box<AbiType>>,
        err: Option<Box<AbiType>>,
    },
    Enum(wit_parser::Int, Vec<String>),
    Variant(wit_parser::Int, Vec<(String, Option<AbiType>)>),
    /// `stream<u8>`: the readable end's handle, a `ReadableStream<Uint8Array>` in TypeScript.
    ByteStream,
    Resource {
        key: u32,
        owned: bool,
    },
    /// A result whose `ok` is a resource handle (a number in TypeScript), with the given flat
    /// layout: `result<response, error-code>` of `wasi:http/handler`. A rejection returns
    /// `err` with the case `error_case` of the error variant carrying `option<string>`
    /// (`internal-error`), the rejection's description.
    OkHandle {
        error_case: u32,
    },
}

/// A statically typed host import from the selected WIT world.
#[derive(Clone, Debug)]
pub struct ResourceDefinition {
    pub key: u32,
    pub module: String,
    pub name: String,
    pub exported: bool,
}
#[derive(Clone, Copy, Debug)]
pub enum ResourceOperation {
    New,
    Representation,
    Drop,
}
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
    pub params: Vec<AbiType>,
    pub indirect_size: Option<u32>,
    pub result: Option<AbiType>,
    pub asynchronous: bool,
}

/// Synchronous vs. callback-lifted async export mode.
#[derive(Clone, Debug)]
pub enum ExportKind {
    Sync,
    Async {
        /// The import module and WIT function name of `task.return`.
        task_return: (String, String),
        /// `wasi:cli/run`'s `run: async func() -> result`, implemented by the top-level
        /// statements: the result is `err` after an uncaught exception or a non-zero exit code.
        command: bool,
    },
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
    pub params: Vec<AbiType>,
    pub indirect_size: Option<u32>,
    pub result: Option<AbiType>,
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

    pub fn is_command(&self) -> bool {
        matches!(self.kind, ExportKind::Async { command: true, .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Flat {
    I32,
    I64,
    F32,
    F64,
}

impl From<WasmType> for Flat {
    fn from(ty: WasmType) -> Self {
        match ty {
            WasmType::I64 | WasmType::PointerOrI64 => Self::I64,
            WasmType::F32 => Self::F32,
            WasmType::F64 => Self::F64,
            WasmType::I32 | WasmType::Pointer | WasmType::Length => Self::I32,
        }
    }
}

impl AbiType {
    /// Whether this type is one of the 11 Canonical ABI scalars.
    pub(super) fn is_scalar(&self) -> bool {
        matches!(self.kind, AbiKind::Scalar(_))
    }

    fn number_name(&self) -> &'static str {
        match self.kind {
            AbiKind::Scalar(Wit::U8) => "u8",
            AbiKind::Scalar(Wit::S8) => "s8",
            AbiKind::Scalar(Wit::U16) => "u16",
            AbiKind::Scalar(Wit::S16) => "s16",
            AbiKind::Scalar(Wit::U32) => "u32",
            AbiKind::Scalar(Wit::S32) => "s32",
            AbiKind::Scalar(Wit::U64) => "u64",
            AbiKind::Scalar(Wit::S64) => "s64",
            AbiKind::Scalar(Wit::F32) => "f32",
            AbiKind::Scalar(Wit::F64) => "f64",
            _ => "own",
        }
    }
    fn direct(&self) -> Result<&[Flat]> {
        self.flat
            .as_deref()
            .ok_or_else(|| internal!("indirect WIT type in a flat signature"))
    }
    /// Exact integer domain supplied by WIT, independent of the source representation.
    pub fn integer_type(&self, ctx: &Context) -> Option<TypeHandle> {
        use Signedness::{Signed, Unsigned};
        let (bits, sign) = match &self.kind {
            AbiKind::Scalar(Wit::S8) => (8, Signed),
            AbiKind::Scalar(Wit::U8) => (8, Unsigned),
            AbiKind::Scalar(Wit::S16) => (16, Signed),
            AbiKind::Scalar(Wit::U16) => (16, Unsigned),
            AbiKind::Scalar(Wit::S32) => (32, Signed),
            AbiKind::Scalar(Wit::U32) => (32, Unsigned),
            AbiKind::Scalar(Wit::S64) => (64, Signed),
            AbiKind::Scalar(Wit::U64) => (64, Unsigned),
            _ => return None,
        };
        Some(IntegerType::get(ctx, bits, sign).into())
    }

    /// The memory access for a scalar, or `None` for compound types.
    fn mem_kind(&self) -> Option<MemKind> {
        Some(match &self.kind {
            AbiKind::Scalar(Wit::Bool | Wit::U8) => MemKind::I32U8,
            AbiKind::Scalar(Wit::S8) => MemKind::I32S8,
            AbiKind::Scalar(Wit::U16) => MemKind::I32U16,
            AbiKind::Scalar(Wit::S16) => MemKind::I32S16,
            AbiKind::Scalar(Wit::S32 | Wit::U32) | AbiKind::Resource { .. } => MemKind::I32,
            AbiKind::Scalar(Wit::S64 | Wit::U64) => MemKind::I64,
            AbiKind::Scalar(Wit::F32) => MemKind::F32,
            AbiKind::Scalar(Wit::F64) => MemKind::F64,
            _ => return None,
        })
    }
}

pub(crate) fn flat_type(ctx: &mut Context, flat: Flat) -> TypeHandle {
    match flat {
        Flat::I32 => i32_type(ctx),
        Flat::I64 => IntegerType::get(ctx, 64, Signedness::Signless).into(),
        Flat::F32 => FP32Type::get(ctx).into(),
        Flat::F64 => FP64Type::get(ctx).into(),
    }
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

fn expect_number(ctx: &Context, mir_ty: TypeHandle, ty: &AbiType) -> Result<()> {
    ensure_diag!(
        mir_ty.deref(ctx).is::<FP64Type>(),
        codes::CONTRACT_MISMATCH,
        "WIT {} needs a TypeScript number",
        ty.number_name()
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

    /// Convert a scalar core value to the MIR value of type `mir_ty`.
    fn lift_scalar(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        raw: Value,
    ) -> Result<Value> {
        if let Some(source) = ty.integer_type(ctx)
            && matches!(repr(ctx, mir_ty)?, Repr::I32 | Repr::I64)
        {
            let value = self.normalize_integer(ctx, raw, source);
            return self.numeric_cast(ctx, value, source, mir_ty);
        }
        let opcode = match &ty.kind {
            AbiKind::Resource { key, owned } => {
                return self.lift_resource(ctx, *key, *owned, mir_ty, raw);
            }
            AbiKind::Scalar(Wit::Bool) => {
                ensure_diag!(
                    repr(ctx, mir_ty)? == Repr::Bool,
                    codes::CONTRACT_MISMATCH,
                    "WIT bool needs a TypeScript boolean"
                );
                let zero = self.i32_const(ctx, 0);
                return Ok(self.num(ctx, NumOpcode::I32Ne, vec![raw, zero]));
            }
            AbiKind::Scalar(Wit::S8 | Wit::S16 | Wit::S32) => NumOpcode::F64ConvertI32S,
            AbiKind::Scalar(Wit::U8 | Wit::U16 | Wit::U32) => NumOpcode::F64ConvertI32U,
            AbiKind::Scalar(Wit::S64 | Wit::U64) => {
                return self.lift_integer(
                    ctx,
                    matches!(ty.kind, AbiKind::Scalar(Wit::S64)),
                    mir_ty,
                    raw,
                );
            }
            AbiKind::Scalar(Wit::F32) => NumOpcode::F64PromoteF32,
            AbiKind::Scalar(Wit::F64) => {
                expect_number(ctx, mir_ty, ty)?;
                return Ok(raw);
            }
            _ => bail_internal!("{ty:?} is not a scalar"),
        };
        expect_number(ctx, mir_ty, ty)?;
        Ok(self.num(ctx, opcode, vec![raw]))
    }

    fn lift_abi(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        mut source: Canonical<'_>,
    ) -> Result<Value> {
        match &ty.kind {
            _ if ty.is_sum() => self.lift_sum(ctx, ty, mir_ty, source),
            AbiKind::String | AbiKind::List(_) => {
                let ptr = source.read(self, ctx, MemKind::I32, 0)?;
                let len = source.read(self, ctx, MemKind::I32, 4)?;
                let value = match &ty.kind {
                    AbiKind::List(element)
                        if element.kind == AbiKind::Scalar(Wit::U8)
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
            AbiKind::Record(fields) => {
                let layout = self.boundary_record(ctx, mir_ty)?;
                let mut values = Vec::with_capacity(fields.len());
                for (name, field_ty) in fields {
                    let at = field_ty.offset;
                    let field = layout.field(ctx, name)?;
                    let lift_mir = scalar_mir_type(ctx, field.ty);
                    let value = self.lift_abi(ctx, field_ty, lift_mir, source.at(at))?;
                    values.push((field, value));
                }
                self.build_record(ctx, &layout, values)
            }
            _ => {
                let kind = ty
                    .mem_kind()
                    .ok_or_else(|| internal!("{ty:?} is not a scalar"))?;
                let raw = source.read(self, ctx, kind, 0)?;
                self.lift_scalar(ctx, ty, mir_ty, raw)
            }
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

    fn boundary_record(
        &mut self,
        ctx: &Context,
        mir_ty: TypeHandle,
    ) -> Result<std::rc::Rc<RecordLayout>> {
        let record = TypedHandle::<RecordType>::from_handle(mir_ty, ctx).map_err(|_| {
            Diagnostic::new(
                codes::CONTRACT_MISMATCH,
                "WIT record needs a TypeScript object type",
            )
        })?;
        ensure_diag!(
            matches!(
                record.deref(ctx).kind(),
                perry_mir::RecordKind::Object
                    | perry_mir::RecordKind::Tuple
                    | perry_mir::RecordKind::DictionaryEntry
            ),
            codes::CONTRACT_MISMATCH,
            "WIT record needs a TypeScript object type"
        );
        self.state.record_layouts.get(ctx, record)
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
        let size = self.i32_const(ctx, layout.size as i32);
        let object = self.record_storage(ctx, layout.record.into(), size)?;
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

    /// Convert a MIR value to a scalar core value.
    fn lower_scalar(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Carriers<Value>,
    ) -> Result<Value> {
        if let Some(target) = ty.integer_type(ctx)
            && (matches!(repr(ctx, mir_ty)?, Repr::I32 | Repr::I64) || is_numeric_type(ctx, mir_ty))
        {
            if matches!(ty.kind, AbiKind::Scalar(Wit::S64 | Wit::U64))
                && crate::numeric::integer(ctx, mir_ty).is_some_and(|(bits, signed)| {
                    bits == 64 && signed != matches!(ty.kind, AbiKind::Scalar(Wit::S64))
                })
            {
                self.check_integer_signedness(ctx, value.scalar()?);
            }
            return self.cast_value(ctx, value, mir_ty, target)?.scalar();
        }
        if matches!(ty.kind, AbiKind::Scalar(Wit::F32 | Wit::F64))
            && (crate::numeric::integer(ctx, mir_ty).is_some_and(|(bits, _)| bits > 1)
                || is_numeric_type(ctx, mir_ty))
        {
            let target = FP64Type::get(ctx).into();
            let value = self.cast_value(ctx, value, mir_ty, target)?.scalar()?;
            return Ok(if matches!(ty.kind, AbiKind::Scalar(Wit::F32)) {
                self.num(ctx, NumOpcode::F32DemoteF64, vec![value])
            } else {
                value
            });
        }
        let value = value.scalar()?;
        let opcode = match &ty.kind {
            AbiKind::Resource { key, owned } => {
                return self.lower_resource(ctx, *key, *owned, mir_ty, value);
            }
            AbiKind::Scalar(Wit::Bool) => return Ok(value),
            AbiKind::Scalar(Wit::S8 | Wit::S16 | Wit::S32 | Wit::U8 | Wit::U16 | Wit::U32) => {
                expect_number(ctx, mir_ty, ty)?;
                let signed = matches!(ty.kind, AbiKind::Scalar(Wit::S8 | Wit::S16 | Wit::S32));
                return Ok(self.int32_bits(ctx, value, signed, None));
            }
            AbiKind::Scalar(Wit::S64 | Wit::U64) => {
                return self.lower_integer(
                    ctx,
                    matches!(ty.kind, AbiKind::Scalar(Wit::S64)),
                    mir_ty,
                    value,
                );
            }
            AbiKind::Scalar(Wit::F32) => NumOpcode::F32DemoteF64,
            AbiKind::Scalar(Wit::F64) => {
                expect_number(ctx, mir_ty, ty)?;
                return Ok(value);
            }
            _ => bail_internal!("{ty:?} is not a scalar"),
        };
        expect_number(ctx, mir_ty, ty)?;
        Ok(self.num(ctx, opcode, vec![value]))
    }

    /// Copy a MIR array into a canonical list buffer and return the `(buffer, length)` pair.
    fn lower_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<(Value, Value)> {
        if element.kind == AbiKind::Scalar(Wit::U8)
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
                if matches!(element.kind, AbiKind::Option(_)) {
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
                element_mir,
                item,
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

    /// Lower a MIR value to its flat core values (appended to `out`).
    /// `values` (`i32`s) in the leading slots of the flat layout `flat`, widened where a
    /// slot is `i64`, and zeros in the others.
    fn flat_i32s(
        &mut self,
        ctx: &mut Context,
        flat: &[Flat],
        values: &[Value],
        out: &mut Vec<Value>,
    ) -> Result<()> {
        for (index, slot) in flat.iter().enumerate() {
            out.push(match (slot, values.get(index)) {
                (Flat::I32, Some(&value)) => value,
                (Flat::I64, Some(&value)) => self.num(ctx, NumOpcode::I64ExtendI32U, vec![value]),
                (Flat::I32, None) => self.i32_const(ctx, 0),
                (Flat::I64, None) => self.i64_const(ctx, 0),
                (Flat::F32 | Flat::F64, _) => bail_internal!("float slots in a handle result"),
            });
        }
        Ok(())
    }

    fn lower_abi(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Carriers<Value>,
        mut target: Canonical<'_>,
    ) -> Result<()> {
        match &ty.kind {
            _ if ty.is_sum() => {
                self.lower_sum(ctx, ty, BoundaryValue::Native(mir_ty, value), target)?
            }
            AbiKind::ByteStream => {
                bail_unsupported!("returning a stream<u8> is not supported yet")
            }
            AbiKind::OkHandle { .. } => {
                let flat = ty.direct()?;
                let Canonical::Flat(out) = target else {
                    bail_internal!("handle results are returned flat");
                };
                // `ok` (0), the handle, and zeros for the rest of the error payload.
                let f64_ty = FP64Type::get(ctx).into();
                let value = if is_numeric_type(ctx, mir_ty) {
                    self.cast_value(ctx, value, mir_ty, f64_ty)?.scalar()?
                } else {
                    expect_number(ctx, mir_ty, ty)?;
                    value.scalar()?
                };
                let zero = self.i32_const(ctx, 0);
                let handle = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![value]);
                self.flat_i32s(ctx, flat, &[zero, handle], out)?;
            }
            AbiKind::String | AbiKind::List(_) => {
                let value = value.scalar()?;
                let (data, len) = match &ty.kind {
                    AbiKind::List(element)
                        if element.kind != AbiKind::Scalar(Wit::U8)
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
                target.write(self, ctx, MemKind::I32, 0, data)?;
                target.write(self, ctx, MemKind::I32, 4, len)?;
            }
            AbiKind::Record(fields) => {
                let value = value.scalar()?;
                let layout = self.boundary_record(ctx, mir_ty)?;
                for (name, field_ty) in fields {
                    let at = field_ty.offset;
                    let field = layout.field(ctx, name)?;
                    let value = self.load_slot_value(ctx, value, field.offset, field.ty)?;
                    self.lower_abi(ctx, field_ty, field.ty, value, target.at(at))?;
                }
            }
            _ => {
                let raw = self.lower_scalar(ctx, ty, mir_ty, value)?;
                let kind = ty
                    .mem_kind()
                    .ok_or_else(|| internal!("{ty:?} is not a scalar"))?;
                target.write(self, ctx, kind, 0, raw)?;
            }
        }
        Ok(())
    }
}
