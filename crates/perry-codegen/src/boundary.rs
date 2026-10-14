//! Canonical ABI conversion for typed component imports and exports.
//!
//! Strings share UTF-8 payloads across the boundary; adopted input buffers and borrowed output
//! owners remain live through lifting/post-return or async acknowledgement. Results support
//! explicit tags and checked completion-style returns/throws. Async exports use stackless P3
//! callbacks; async imports retain a canonical transfer until the host settles it.
use crate::{
    function::FunctionLowering,
    host::{EffectHost, Foreign, ValueHost},
    module::ModuleState,
    repr::{Repr, i32_type, repr, slot_kind, wasm_type},
    runtime::Runtime,
};
use anyhow::{Result, anyhow, bail, ensure};
use perry_abi::layout::{array as layout, record, string as strings};
use perry_mir::types::{ArrayType, RecordType};
use pliron::{
    builtin::{
        op_interfaces::{SingleBlockRegionInterface, SymbolOpInterface},
        ops::FuncOp,
        type_interfaces::FunctionTypeInterface,
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
    control::{ReturnOp, UnreachableOp},
    ops::{CallOp, set_export},
};

mod exports;
mod imports;
mod integers;
mod parameters;
mod resources;
mod scopes;
mod sums;

pub(super) use exports::export;
pub(super) use resources::{emit_drain, used_keys};
pub(super) use scopes::Scope;

/// A WIT value type as the canonical ABI sees it.
#[derive(Clone, Debug, PartialEq)]
pub enum AbiType {
    Bool,
    S8,
    U8,
    S16,
    U16,
    S32,
    U32,
    S64,
    U64,
    F32,
    F64,
    String,
    List(Box<AbiType>),
    /// Fields in WIT order, named as the TypeScript record fields.
    Record(Vec<(String, AbiType)>),
    Option(Box<AbiType>),
    Result {
        ok: Option<Box<AbiType>>,
        err: Option<Box<AbiType>>,
    },
    Enum(Vec<String>),
    Variant(Vec<(String, Option<AbiType>)>),
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
        flat: Vec<Flat>,
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
#[derive(Clone, Copy, Debug)]
pub enum ImportKind {
    Function,
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
    /// Core export name: the WIT function name, or `interface#function`.
    pub core_name: String,
    /// The MIR function implementing it.
    pub function: String,
    pub params: Vec<AbiType>,
    pub result: Option<AbiType>,
    pub kind: ExportKind,
}

impl Export {
    pub fn is_async(&self) -> bool {
        matches!(self.kind, ExportKind::Async { .. })
    }

    pub fn is_command(&self) -> bool {
        matches!(self.kind, ExportKind::Async { command: true, .. })
    }
}

const MAX_FLAT_PARAMS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Flat {
    I32,
    I64,
    F32,
    F64,
}

impl AbiType {
    /// Exact integer domain supplied by WIT, independent of the source representation.
    pub fn integer_type(&self, ctx: &Context) -> Option<TypeHandle> {
        use Signedness::{Signed, Unsigned};
        let (bits, sign) = match self {
            Self::S8 => (8, Signed),
            Self::U8 => (8, Unsigned),
            Self::S16 => (16, Signed),
            Self::U16 => (16, Unsigned),
            Self::S32 => (32, Signed),
            Self::U32 => (32, Unsigned),
            Self::S64 => (64, Signed),
            Self::U64 => (64, Unsigned),
            _ => return None,
        };
        Some(IntegerType::get(ctx, bits, sign).into())
    }

    fn flatten(&self, out: &mut Vec<Flat>) {
        match self {
            AbiType::Bool
            | AbiType::S8
            | AbiType::U8
            | AbiType::S16
            | AbiType::U16
            | AbiType::S32
            | AbiType::U32
            | AbiType::ByteStream
            | AbiType::Resource { .. } => out.push(Flat::I32),
            AbiType::S64 | AbiType::U64 => out.push(Flat::I64),
            AbiType::F32 => out.push(Flat::F32),
            AbiType::F64 => out.push(Flat::F64),
            AbiType::String | AbiType::List(_) => out.extend([Flat::I32, Flat::I32]),
            AbiType::Record(fields) => fields.iter().for_each(|(_, ty)| ty.flatten(out)),
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => out.extend(self.sum_flat()),
            AbiType::OkHandle { flat, .. } => out.extend(flat.iter().copied()),
        }
    }

    fn flat(&self) -> Vec<Flat> {
        let mut out = Vec::new();
        self.flatten(&mut out);
        out
    }

    fn alignment(&self) -> u32 {
        match self {
            AbiType::Bool | AbiType::S8 | AbiType::U8 => 1,
            AbiType::S16 | AbiType::U16 => 2,
            AbiType::S32
            | AbiType::U32
            | AbiType::F32
            | AbiType::ByteStream
            | AbiType::Resource { .. } => 4,
            AbiType::S64 | AbiType::U64 | AbiType::F64 => 8,
            AbiType::String | AbiType::List(_) => 4,
            AbiType::Record(fields) => fields.iter().map(|(_, t)| t.alignment()).max().unwrap_or(1),
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => self.sum_alignment(),
            AbiType::OkHandle { .. } => 8,
        }
    }

    fn size(&self) -> u32 {
        match self {
            AbiType::Bool | AbiType::S8 | AbiType::U8 => 1,
            AbiType::S16 | AbiType::U16 => 2,
            AbiType::S32
            | AbiType::U32
            | AbiType::F32
            | AbiType::ByteStream
            | AbiType::Resource { .. } => 4,
            AbiType::S64 | AbiType::U64 | AbiType::F64 => 8,
            AbiType::String | AbiType::List(_) => 8,
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => self.sum_size(),
            AbiType::OkHandle { flat, .. } => 8 * flat.len() as u32,
            AbiType::Record(fields) => record_size(fields.iter().map(|(_, ty)| ty)),
        }
    }

    fn field_offsets(&self) -> impl Iterator<Item = u32> + '_ {
        let fields = match self {
            AbiType::Record(fields) => fields.as_slice(),
            _ => &[],
        };
        record_offsets(fields.iter().map(|(_, ty)| ty))
    }

    /// The memory access for a scalar, or `None` for compound types.
    fn mem_kind(&self) -> Option<MemKind> {
        Some(match self {
            AbiType::Bool | AbiType::U8 => MemKind::I32U8,
            AbiType::S8 => MemKind::I32S8,
            AbiType::U16 => MemKind::I32U16,
            AbiType::S16 => MemKind::I32S16,
            AbiType::S32 | AbiType::U32 | AbiType::Resource { .. } => MemKind::I32,
            AbiType::S64 | AbiType::U64 => MemKind::I64,
            AbiType::F32 => MemKind::F32,
            AbiType::F64 => MemKind::F64,
            _ => return None,
        })
    }
}

fn align(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

fn record_offsets<'a>(types: impl Iterator<Item = &'a AbiType>) -> impl Iterator<Item = u32> {
    types.scan(0, |offset, ty| {
        let at = align(*offset, ty.alignment());
        *offset = at + ty.size();
        Some(at)
    })
}

fn record_size<'a>(types: impl Iterator<Item = &'a AbiType>) -> u32 {
    let (end, alignment) = types.fold((0, 1), |(end, alignment), ty| {
        (
            align(end, ty.alignment()) + ty.size(),
            alignment.max(ty.alignment()),
        )
    });
    align(end, alignment)
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

fn expect_number(ctx: &Context, mir_ty: TypeHandle, ty: &AbiType) -> Result<()> {
    ensure!(
        mir_ty.deref(ctx).is::<FP64Type>(),
        "WIT {ty:?} needs a TypeScript number"
    );
    Ok(())
}

impl FunctionLowering<'_, '_> {
    /// Acknowledge every outstanding host transfer before releasing a cancelled task.
    fn abort_task(&mut self, ctx: &mut Context) -> Result<()> {
        self.runtime(ctx, Runtime::TaskCancel, vec![]);
        self.cleanup_foreign(ctx)?;
        if self.state.uses_fs {
            self.runtime(ctx, Runtime::FsClose, vec![]);
        }
        for (used, function) in [
            (self.state.uses_http_server, Runtime::WebServeClose),
            (self.state.uses_fetch, Runtime::WebFetchClose),
            (self.state.uses_streams, Runtime::WebStreamsClose),
            (self.state.uses_io, Runtime::IoCancel),
        ] {
            if used {
                self.runtime_void(ctx, function, vec![]);
            }
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
        let opcode = match ty {
            AbiType::Resource { key, owned } => {
                return self.lift_resource(ctx, *key, *owned, mir_ty, raw);
            }
            AbiType::Bool => {
                ensure!(
                    repr(ctx, mir_ty)? == Repr::Bool,
                    "WIT bool needs a TypeScript boolean"
                );
                let zero = self.i32_const(ctx, 0);
                return Ok(self.num(ctx, NumOpcode::I32Ne, vec![raw, zero]));
            }
            AbiType::S8 | AbiType::S16 | AbiType::S32 => NumOpcode::F64ConvertI32S,
            AbiType::U8 | AbiType::U16 | AbiType::U32 => NumOpcode::F64ConvertI32U,
            AbiType::S64 | AbiType::U64 => {
                return self.lift_integer(ctx, matches!(ty, AbiType::S64), mir_ty, raw);
            }
            AbiType::F32 => NumOpcode::F64PromoteF32,
            AbiType::F64 => {
                expect_number(ctx, mir_ty, ty)?;
                return Ok(raw);
            }
            _ => bail!("{ty:?} is not a scalar"),
        };
        expect_number(ctx, mir_ty, ty)?;
        Ok(self.num(ctx, opcode, vec![raw]))
    }

    /// Lift a value from flat core values (consumed from the end of `flat`).
    fn lift_flat(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        flat: &mut Vec<Value>,
    ) -> Result<Value> {
        let mut next = || flat.pop().ok_or_else(|| anyhow!("too few flat values"));
        match ty {
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => self.lift_sum_flat(ctx, ty, mir_ty, flat),
            AbiType::String => {
                let (ptr, len) = (next()?, next()?);
                self.lift_string(ctx, mir_ty, ptr, len)
            }
            AbiType::List(element) => {
                let (ptr, len) = (next()?, next()?);
                self.lift_list(ctx, element, mir_ty, ptr, len)
            }
            AbiType::ByteStream => {
                let handle = next()?;
                self.lift_stream(ctx, mir_ty, handle)
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                let mut values = Vec::new();
                for (name, field_ty) in fields {
                    let (_, field_mir) = record_field(ctx, record, name)?;
                    values.push((
                        name.as_str(),
                        self.lift_flat(ctx, field_ty, field_mir, flat)?,
                    ));
                }
                self.build_record(ctx, mir_ty, values)
            }
            scalar => {
                let raw = next()?;
                self.lift_scalar(ctx, scalar, mir_ty, raw)
            }
        }
    }

    /// Lift a value stored at `address + offset` in canonical layout.
    fn lift_load(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        address: Value,
        offset: u32,
    ) -> Result<Value> {
        match ty {
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => self.lift_sum_load(ctx, ty, mir_ty, address, offset),
            AbiType::String => {
                let ptr = self.load(ctx, MemKind::I32, address, offset);
                let len = self.load(ctx, MemKind::I32, address, offset + 4);
                self.lift_string(ctx, mir_ty, ptr, len)
            }
            AbiType::List(element) => {
                let ptr = self.load(ctx, MemKind::I32, address, offset);
                let len = self.load(ctx, MemKind::I32, address, offset + 4);
                self.lift_list(ctx, element, mir_ty, ptr, len)
            }
            AbiType::ByteStream => {
                let handle = self.load(ctx, MemKind::I32, address, offset);
                self.lift_stream(ctx, mir_ty, handle)
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                let mut values = Vec::new();
                for ((name, field_ty), at) in fields.iter().zip(ty.field_offsets()) {
                    let (_, field_mir) = record_field(ctx, record, name)?;
                    let value = self.lift_load(ctx, field_ty, field_mir, address, offset + at)?;
                    values.push((name.as_str(), value));
                }
                self.build_record(ctx, mir_ty, values)
            }
            scalar => {
                let kind = scalar
                    .mem_kind()
                    .ok_or_else(|| anyhow!("{scalar:?} is not a scalar"))?;
                let raw = self.load(ctx, kind, address, offset);
                self.lift_scalar(ctx, scalar, mir_ty, raw)
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
        ensure!(
            mir_ty.deref(ctx).is::<perry_mir::types::ReadableType>(),
            "WIT stream<u8> needs a TypeScript ReadableStream<Uint8Array>"
        );
        self.state.uses_streams = true;
        Ok(self.runtime(ctx, Runtime::WebReadableNew, vec![handle]))
    }

    fn lift_string(
        &mut self,
        ctx: &mut Context,
        mir_ty: TypeHandle,
        ptr: Value,
        len: Value,
    ) -> Result<Value> {
        ensure!(
            mir_ty.deref(ctx).is::<perry_mir::types::StringType>(),
            "WIT string needs a TypeScript string"
        );
        let string = self.runtime(ctx, Runtime::StringAdopt, vec![ptr, len]);
        if self.state.collectible {
            self.runtime_void(ctx, Runtime::CabiRelease, vec![ptr]);
        }
        Ok(string)
    }

    fn lift_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        ptr: Value,
        len: Value,
    ) -> Result<Value> {
        if *element == AbiType::U8 && mir_ty.deref(ctx).is::<perry_mir::types::BytesType>() {
            let bytes = self.runtime(ctx, Runtime::StdBytesAdopt, vec![len, ptr]);
            if self.state.collectible {
                self.runtime_void(ctx, Runtime::CabiRelease, vec![ptr]);
            }
            return Ok(bytes);
        }
        let element_mir = TypedHandle::<ArrayType>::from_handle(mir_ty, ctx)
            .map_err(|_| anyhow!("WIT list needs a TypeScript array"))?
            .deref(ctx)
            .element();
        let kind = slot_kind(ctx, element_mir)?;
        let array = self.new_array(ctx, len, element_mir)?;
        let data = self.load(ctx, MemKind::I32, array, layout::DATA);
        let size = element.size() as i32;
        self.counted_loop(ctx, len, |b, ctx, i| {
            let stride = b.i32_const(ctx, size);
            let offset = b.num(ctx, NumOpcode::I32Mul, vec![i, stride]);
            let source = b.num(ctx, NumOpcode::I32Add, vec![ptr, offset]);
            let value = b.lift_load(ctx, element, element_mir, source, 0)?;
            let slot = b.slot_address(ctx, data, i);
            b.store(ctx, kind, slot, 0, value);
            Ok(())
        })?;
        self.store(ctx, MemKind::I32, array, layout::LEN, len);
        if self.state.collectible {
            self.runtime_void(ctx, Runtime::CabiRelease, vec![ptr]);
        }
        Ok(array)
    }

    fn record_type(&self, ctx: &Context, mir_ty: TypeHandle) -> Result<TypedHandle<RecordType>> {
        TypedHandle::<RecordType>::from_handle(mir_ty, ctx)
            .map_err(|_| anyhow!("WIT record needs a TypeScript object type"))
    }

    /// Allocate a MIR record from named field values.
    fn build_record(
        &mut self,
        ctx: &mut Context,
        mir_ty: TypeHandle,
        values: Vec<(&str, Value)>,
    ) -> Result<Value> {
        let record = self.record_type(ctx, mir_ty)?;
        let field_count = record
            .deref(ctx)
            .fields()
            .map_err(|_| anyhow!("record without fields"))?
            .len();
        ensure!(
            values.len() == field_count,
            "the WIT record has {} fields but the TypeScript type {} has {field_count}",
            values.len(),
            record.deref(ctx).name()
        );
        let size = self.i32_const(ctx, record::field(field_count) as i32);
        let object = self.record_storage(ctx, record.into(), size)?;
        self.record_header(ctx, object, record);
        for (field, value) in values {
            let (index, field_mir) = record_field(ctx, record, field)?;
            let kind = slot_kind(ctx, field_mir)?;
            self.store(ctx, kind, object, record::field(index), value);
        }
        Ok(object)
    }

    /// Convert a MIR value to a scalar core value.
    fn lower_flat(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<Value> {
        if let Some(target) = ty.integer_type(ctx)
            && matches!(repr(ctx, mir_ty)?, Repr::I32 | Repr::I64)
        {
            return self.numeric_cast(ctx, value, mir_ty, target);
        }
        if matches!(ty, AbiType::F32 | AbiType::F64)
            && crate::numeric::integer(ctx, mir_ty).is_some_and(|(bits, _)| bits > 1)
        {
            let target = FP64Type::get(ctx).into();
            let value = self.numeric_cast(ctx, value, mir_ty, target)?;
            return Ok(if matches!(ty, AbiType::F32) {
                self.num(ctx, NumOpcode::F32DemoteF64, vec![value])
            } else {
                value
            });
        }
        if ty.is_sum() {
            let mut flat = self.lower_sum_flat(ctx, ty, mir_ty, value)?;
            ensure!(
                flat.len() == 1,
                "compound result requires an indirect return"
            );
            return Ok(flat.remove(0));
        }
        let opcode = match ty {
            AbiType::Resource { key, owned } => {
                return self.lower_resource(ctx, *key, *owned, mir_ty, value);
            }
            AbiType::Bool => return Ok(value),
            AbiType::S8 | AbiType::S16 | AbiType::S32 => NumOpcode::I32TruncSatF64S,
            AbiType::U8 | AbiType::U16 | AbiType::U32 => NumOpcode::I32TruncSatF64U,
            AbiType::S64 | AbiType::U64 => {
                return self.lower_integer(ctx, matches!(ty, AbiType::S64), mir_ty, value);
            }
            AbiType::F32 => NumOpcode::F32DemoteF64,
            AbiType::F64 => {
                expect_number(ctx, mir_ty, ty)?;
                return Ok(value);
            }
            _ => bail!("{ty:?} is not a scalar"),
        };
        expect_number(ctx, mir_ty, ty)?;
        Ok(self.num(ctx, opcode, vec![value]))
    }

    /// Store a MIR value at `address + offset` in canonical layout.
    fn lower_store(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
        address: Value,
        offset: u32,
    ) -> Result<()> {
        match ty {
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => {
                self.lower_sum_store(ctx, ty, mir_ty, value, address, offset)?
            }
            AbiType::ByteStream => bail!("returning a stream<u8> is not supported yet"),
            AbiType::OkHandle { .. } => bail!("handle results are returned flat"),
            AbiType::String => {
                ensure!(
                    mir_ty.deref(ctx).is::<perry_mir::types::StringType>(),
                    "WIT string needs a TypeScript string"
                );
                let data = self.load(ctx, MemKind::I32, value, strings::DATA);
                let len = self.load(ctx, MemKind::I32, value, strings::LEN);
                self.store(ctx, MemKind::I32, address, offset, data);
                self.store(ctx, MemKind::I32, address, offset + 4, len);
            }
            AbiType::List(element) => {
                let (buffer, len) = self.lower_list(ctx, element, mir_ty, value)?;
                self.store(ctx, MemKind::I32, address, offset, buffer);
                self.store(ctx, MemKind::I32, address, offset + 4, len);
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                for ((name, field_ty), at) in fields.iter().zip(ty.field_offsets()) {
                    let (index, field_mir) = record_field(ctx, record, name)?;
                    let kind = slot_kind(ctx, field_mir)?;
                    let field = self.load(ctx, kind, value, record::field(index));
                    self.lower_store(ctx, field_ty, field_mir, field, address, offset + at)?;
                }
            }
            scalar => {
                let raw = self.lower_flat(ctx, scalar, mir_ty, value)?;
                let kind = scalar
                    .mem_kind()
                    .ok_or_else(|| anyhow!("{scalar:?} is not a scalar"))?;
                self.store(ctx, kind, address, offset, raw);
            }
        }
        Ok(())
    }

    /// Copy a MIR array into a canonical list buffer; returns (buffer, length).
    fn lower_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<(Value, Value)> {
        if *element == AbiType::U8 {
            if mir_ty.deref(ctx).is::<perry_mir::types::StringType>() {
                return Ok((
                    self.load(ctx, MemKind::I32, value, strings::DATA),
                    self.load(ctx, MemKind::I32, value, strings::LEN),
                ));
            }
            if mir_ty.deref(ctx).is::<perry_mir::types::BytesType>()
                || mir_ty.deref(ctx).is::<perry_mir::types::ByteInputType>()
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
        }
        let element_mir = TypedHandle::<ArrayType>::from_handle(mir_ty, ctx)
            .map_err(|_| anyhow!("WIT list needs a TypeScript array"))?
            .deref(ctx)
            .element();
        let kind = slot_kind(ctx, element_mir)?;
        let len = self.load(ctx, MemKind::I32, value, layout::LEN);
        let data = self.load(ctx, MemKind::I32, value, layout::DATA);
        let stride = self.i32_const(ctx, element.size() as i32);
        let bytes = self.num(ctx, NumOpcode::I32Mul, vec![len, stride]);
        let buffer = self.alloc(ctx, bytes);
        let size = element.size() as i32;
        self.counted_loop(ctx, len, |b, ctx, i| {
            let slot = b.slot_address(ctx, data, i);
            let item = b.load(ctx, kind, slot, 0);
            let stride = b.i32_const(ctx, size);
            let at = b.num(ctx, NumOpcode::I32Mul, vec![i, stride]);
            let target = b.num(ctx, NumOpcode::I32Add, vec![buffer, at]);
            b.lower_store(ctx, element, element_mir, item, target, 0)
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
                (Flat::F32 | Flat::F64, _) => bail!("float slots in a handle result"),
            });
        }
        Ok(())
    }

    fn lower_flat_values(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
        out: &mut Vec<Value>,
    ) -> Result<()> {
        match ty {
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => out.extend(self.lower_sum_flat(ctx, ty, mir_ty, value)?),
            AbiType::ByteStream => bail!("returning a stream<u8> is not supported yet"),
            AbiType::OkHandle { flat, .. } => {
                // `ok` (0), the handle, and zeros for the rest of the error payload.
                expect_number(ctx, mir_ty, ty)?;
                let zero = self.i32_const(ctx, 0);
                let handle = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![value]);
                self.flat_i32s(ctx, flat, &[zero, handle], out)?;
            }
            AbiType::String => {
                ensure!(
                    mir_ty.deref(ctx).is::<perry_mir::types::StringType>(),
                    "WIT string needs a TypeScript string"
                );
                out.push(self.load(ctx, MemKind::I32, value, strings::DATA));
                out.push(self.load(ctx, MemKind::I32, value, strings::LEN));
            }
            AbiType::List(element) => {
                let (buffer, len) = self.lower_list(ctx, element, mir_ty, value)?;
                out.extend([buffer, len]);
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                for (name, field_ty) in fields {
                    let (index, field_mir) = record_field(ctx, record, name)?;
                    let kind = slot_kind(ctx, field_mir)?;
                    let field = self.load(ctx, kind, value, record::field(index));
                    self.lower_flat_values(ctx, field_ty, field_mir, field, out)?;
                }
            }
            scalar => out.push(self.lower_flat(ctx, scalar, mir_ty, value)?),
        }
        Ok(())
    }
}

/// Slot index and MIR type of the TypeScript field `name`.
fn record_field(
    ctx: &Context,
    record: TypedHandle<RecordType>,
    name: &str,
) -> Result<(usize, TypeHandle)> {
    let record = record.deref(ctx);
    record
        .field(name)
        .ok_or_else(|| anyhow!("TypeScript type {} has no field {name}", record.name()))
}
