//! Canonical discriminants and payloads for closed WIT sums.
use super::*;
use perry_mir::{absence, optional_value, present_type};

impl AbiType {
    /// Explicit tagged results take precedence over completion-style returns.
    pub(super) fn is_completion(&self, ctx: &Context, mir: Option<TypeHandle>) -> bool {
        let Self::Result { ok, err } = self else {
            return false;
        };
        let Some(record) = mir.and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
        else {
            return true;
        };
        let tagged = record_field(ctx, record, "ok")
            .is_ok_and(|(_, ty)| repr(ctx, ty).is_ok_and(|r| r == Repr::Bool))
            && ok
                .as_ref()
                .is_none_or(|_| record_field(ctx, record, "value").is_ok())
            && err
                .as_ref()
                .is_none_or(|_| record_field(ctx, record, "error").is_ok());
        !tagged
    }

    pub(crate) fn accepts_exception(&self) -> bool {
        match self {
            Self::Result { err, .. } => err.as_deref().is_none_or(|err| match err {
                Self::Bool
                | Self::S8
                | Self::U8
                | Self::S16
                | Self::U16
                | Self::S32
                | Self::U32
                | Self::S64
                | Self::U64
                | Self::F32
                | Self::F64
                | Self::String
                | Self::Enum(_) => true,
                Self::Record(fields) => {
                    fields.len() == 2
                        && ["name", "message"].iter().all(|name| {
                            fields
                                .iter()
                                .any(|(field, ty)| field == name && *ty == Self::String)
                        })
                }
                _ => false,
            }),
            Self::OkHandle { .. } => true,
            _ => false,
        }
    }
    pub(super) fn is_sum(&self) -> bool {
        matches!(
            self,
            Self::Option(_)
                | Self::Absent { .. }
                | Self::Result { .. }
                | Self::Enum(_)
                | Self::Variant(_)
        )
    }

    pub(super) fn cases(&self) -> impl ExactSizeIterator<Item = Option<&Self>> {
        let count = match self {
            Self::Variant(cases) => cases.len(),
            Self::Option(_) | Self::Result { .. } => 2,
            Self::Absent { null, .. } => {
                if *null {
                    3
                } else {
                    2
                }
            }
            Self::Enum(names) => names.len(),
            _ => 0,
        };
        (0..count).map(|case| match self {
            Self::Variant(cases) => cases[case].1.as_ref(),
            Self::Option(inner) => (case == 1).then_some(inner.as_ref()),
            Self::Absent { value, null } => {
                (case == if *null { 2 } else { 1 }).then_some(value.as_ref())
            }
            Self::Result { ok, err } => {
                if case == 0 {
                    ok.as_deref()
                } else {
                    err.as_deref()
                }
            }
            _ => None,
        })
    }

    /// How the case index is stored in memory.
    pub(super) fn discriminant_kind(&self) -> MemKind {
        match self.cases().len() {
            0..=256 => MemKind::I32U8,
            257..=65536 => MemKind::I32U16,
            _ => MemKind::I32,
        }
    }

    pub(super) fn discriminant(&self) -> AbiType {
        match self.cases().len() {
            0..=256 => Self::U8,
            257..=65536 => Self::U16,
            _ => Self::U32,
        }
    }

    pub(super) fn sum_flat(&self) -> Vec<Flat> {
        let mut flat = vec![Flat::I32];
        for case in self.cases().flatten() {
            for (index, ty) in case.flat().into_iter().enumerate() {
                if let Some(slot) = flat.get_mut(index + 1) {
                    *slot = match (*slot, ty) {
                        (a, b) if a == b => a,
                        (Flat::I32 | Flat::F32, Flat::I32 | Flat::F32) => Flat::I32,
                        _ => Flat::I64,
                    };
                } else {
                    flat.push(ty);
                }
            }
        }
        flat
    }

    pub(super) fn sum_alignment(&self) -> u32 {
        self.cases()
            .flatten()
            .map(Self::alignment)
            .fold(self.discriminant().alignment(), u32::max)
    }

    pub(super) fn payload_offset(&self) -> u32 {
        align(self.discriminant().size(), self.sum_alignment())
    }

    pub(super) fn sum_size(&self) -> u32 {
        let payload = self.cases().flatten().map(Self::size).max().unwrap_or(0);
        align(self.payload_offset() + payload, self.sum_alignment())
    }
}

#[derive(Clone, Copy)]
pub(super) enum BoundaryValue {
    Native(TypeHandle, Carriers<Value>),
    Completion {
        case: usize,
        payload: Option<(TypeHandle, Carriers<Value>)>,
    },
}

struct NativePayload {
    mir: TypeHandle,
    slot: Option<(u32, TypeHandle)>,
}

impl FunctionLowering<'_, '_> {
    /// Thrown scalars are checked against the declared WIT error payload before lowering.
    pub(super) fn completion_error(
        &mut self,
        ctx: &mut Context,
        result: &AbiType,
        exception: Value,
    ) -> Result<Option<(TypeHandle, Carriers<Value>)>> {
        use perry_abi::layout::exception as exception_layout;
        let AbiType::Result { err, .. } = result else {
            bail!("completion requires a WIT result");
        };
        let Some(err) = err.as_deref() else {
            return Ok(None);
        };
        if let AbiType::Record(fields) = err {
            ensure!(
                fields.len() == 2
                    && ["name", "message"].iter().all(|name| fields
                        .iter()
                        .any(|(field, ty)| field == name && *ty == AbiType::String)),
                "completion-style Error objects require a {{name: string, message: string}} WIT error; return an explicit tagged result for {err:?}"
            );
            let kind = self.load(ctx, MemKind::I32, exception, exception_layout::KIND);
            let minimum = self.i32_const(ctx, exception_layout::TYPE_ERROR as i32);
            let maximum = self.i32_const(ctx, exception_layout::ERROR as i32);
            let at_least = self.num(ctx, NumOpcode::I32GeU, vec![kind, minimum]);
            let at_most = self.num(ctx, NumOpcode::I32LeU, vec![kind, maximum]);
            let basic = self.num(ctx, NumOpcode::I32And, vec![at_least, at_most]);
            let six = self.i32_const(ctx, 6);
            let offset = self.num(ctx, NumOpcode::I32Sub, vec![kind, six]);
            let two = self.i32_const(ctx, 2);
            let native = self.num(ctx, NumOpcode::I32LtU, vec![offset, two]);
            let matches = self.num(ctx, NumOpcode::I32Or, vec![basic, native]);
            self.trap_unless(ctx, matches, true);
            let string = perry_mir::StringType::get(ctx).into();
            let mir = RecordType::get(
                ctx,
                identifier("perry_completion_error"),
                Some(vec![("message".into(), string), ("name".into(), string)]),
            )
            .map_err(|_| anyhow!("inconsistent completion Error record shape"))?
            .into();
            let name = self.runtime(ctx, Runtime::StdExceptionName, vec![exception]);
            let message = self.runtime(ctx, Runtime::ExceptionMessage, vec![exception]);
            let value = self.build_record(ctx, mir, vec![("name", name), ("message", message)])?;
            return Ok(Some((mir, Carriers::Scalar(value))));
        }
        let (expected, mir, kind) = match err {
            AbiType::String | AbiType::Enum(_) => (
                exception_layout::TAG_STRING,
                perry_mir::StringType::get(ctx).into(),
                MemKind::I32,
            ),
            AbiType::S64 | AbiType::U64 => (
                exception_layout::TAG_BIGINT,
                perry_mir::BigIntType::get(ctx).into(),
                MemKind::I32,
            ),
            AbiType::Bool => (
                exception_layout::TAG_BOOLEAN,
                IntegerType::get(ctx, 1, Signedness::Signless).into(),
                MemKind::I32,
            ),
            AbiType::F32
            | AbiType::F64
            | AbiType::S8
            | AbiType::U8
            | AbiType::S16
            | AbiType::U16
            | AbiType::S32
            | AbiType::U32 => (
                exception_layout::TAG_NUMBER,
                FP64Type::get(ctx).into(),
                MemKind::F64,
            ),
            _ => bail!(
                "completion-style throws require a scalar WIT error; return an explicit tagged result for {err:?}"
            ),
        };
        let tag = self.load(ctx, MemKind::I32, exception, exception_layout::TAG);
        let expected = self.i32_const(ctx, expected as i32);
        let matches = self.num(ctx, NumOpcode::I32Eq, vec![tag, expected]);
        self.trap_unless(ctx, matches, true);
        let value = self.load(ctx, kind, exception, exception_layout::VALUE);
        if self.state.collectible && repr(ctx, mir)? == Repr::Ref {
            self.runtime(ctx, Runtime::GcPin, vec![value]);
        }
        Ok(Some((mir, Carriers::Scalar(value))))
    }
    pub(super) fn sum_switch<'a>(
        &mut self,
        ctx: &mut Context,
        tag: Value,
        cases: impl ExactSizeIterator<Item = Option<&'a AbiType>>,
        results: Vec<TypeHandle>,
        mut case: impl FnMut(&mut Self, &mut Context, usize, Option<&'a AbiType>) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let join = self.new_block(ctx, results);
        for (index, payload) in cases.enumerate() {
            let selected = self.new_block(ctx, vec![]);
            let next = self.new_block(ctx, vec![]);
            let discriminant = self.i32_const(ctx, index as i32);
            let matches = self.num(ctx, NumOpcode::I32Eq, vec![tag, discriminant]);
            self.cond_br(ctx, matches, (selected, vec![]), (next, vec![]));
            self.block = selected;
            let values = case(self, ctx, index, payload)?;
            self.br(ctx, join, values);
            self.block = next;
        }
        self.trap(ctx);
        self.block = join;
        Ok(join.deref(ctx).arguments().collect())
    }

    fn native_payload(
        &self,
        ctx: &Context,
        ty: &AbiType,
        mir: TypeHandle,
        case: usize,
        abi: &AbiType,
    ) -> Result<NativePayload> {
        let (offset, field) = match ty {
            AbiType::Option(_) | AbiType::Absent { .. } => {
                return Ok(NativePayload {
                    mir: optional_value(ctx, mir)
                        .ok_or_else(|| anyhow!("WIT option needs a nullable TypeScript value"))?,
                    slot: None,
                });
            }
            AbiType::Result { .. } => {
                let record = self.record_type(ctx, mir)?;
                Self::record_type_field_slot(
                    ctx,
                    record,
                    if case == 0 { "value" } else { "error" },
                )?
            }
            AbiType::Variant(cases) => {
                let variant = TypedHandle::<perry_mir::VariantType>::from_handle(mir, ctx)?;
                let field = variant
                    .deref(ctx)
                    .cases()
                    .iter()
                    .find(|(name, _)| *name == cases[case].0)
                    .and_then(|(_, payload)| *payload)
                    .ok_or_else(|| anyhow!("variant payload is missing"))?;
                (perry_abi::layout::variant::VALUE, field)
            }
            _ => bail!("sum has no payload"),
        };
        let mir = if matches!(ty, AbiType::Result { .. })
            && !matches!(abi, AbiType::Option(_) | AbiType::Absent { .. })
        {
            present_type(ctx, field)
        } else {
            field
        };
        Ok(NativePayload {
            mir,
            slot: Some((offset, field)),
        })
    }

    fn lift_case(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        case: usize,
        payload: Option<(TypeHandle, Value)>,
    ) -> Result<Value> {
        match ty {
            AbiType::Absent { null, .. } => match payload {
                Some((_, value)) => Ok(value),
                None if *null && case == 0 => self.null(ctx, mir),
                None => self.undefined(ctx, mir),
            },
            AbiType::Variant(cases) => self.variant_new(ctx, &cases[case].0, payload),
            AbiType::Enum(names) => {
                ensure!(
                    mir.deref(ctx).is::<perry_mir::StringType>(),
                    "WIT enum needs a string literal union"
                );
                let text = self.state.data.string(&names[case]);
                Ok(self.i32_const(ctx, text as i32))
            }
            AbiType::Option(_) => {
                ensure!(
                    optional_value(ctx, mir).is_some(),
                    "WIT option needs a nullable TypeScript value"
                );
                match payload {
                    Some((_, value)) => Ok(value),
                    None if absence(ctx, mir).is_some_and(|a| a.admits_null()) => {
                        self.null(ctx, mir)
                    }
                    None => self.undefined(ctx, mir),
                }
            }
            AbiType::Result { .. } => {
                let record = self.record_type(ctx, mir)?;
                let fields = record
                    .deref(ctx)
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?
                    .to_vec();
                let mut values = Vec::new();
                for (name, field) in &fields {
                    let value = if name.as_str() == "ok" {
                        ensure!(
                            repr(ctx, *field)? == Repr::Bool,
                            "result ok field must be boolean"
                        );
                        self.i32_const(ctx, i32::from(case == 0))
                    } else if name.as_str() == if case == 0 { "value" } else { "error" } {
                        payload
                            .ok_or_else(|| {
                                anyhow!("payload-free result has an unexpected {name} field")
                            })?
                            .1
                    } else {
                        ensure!(
                            absence(ctx, *field).is_some_and(|a| a.admits_undefined()),
                            "inactive result field {name} must admit undefined"
                        );
                        let field_lift = scalar_mir_type(ctx, *field);
                        self.undefined(ctx, field_lift)?
                    };
                    values.push((name.as_str(), value));
                }
                self.build_record(ctx, mir, values)
            }
            other => bail!("WIT {other:?} is not a sum type"),
        }
    }

    /// Reinterpret canonical joined slots, preserving the low payload bits when widening.
    pub(super) fn coerce_flat(
        &mut self,
        ctx: &mut Context,
        value: Value,
        from: Flat,
        to: Flat,
    ) -> Value {
        if from == to {
            return value;
        }
        let (value, from) = match from {
            Flat::F32 => (
                self.num(ctx, NumOpcode::I32ReinterpretF32, vec![value]),
                Flat::I32,
            ),
            Flat::F64 => (
                self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]),
                Flat::I64,
            ),
            ty => (value, ty),
        };
        let width = if matches!(to, Flat::I32 | Flat::F32) {
            Flat::I32
        } else {
            Flat::I64
        };
        let value = match (from, width) {
            (Flat::I32, Flat::I64) => self.num(ctx, NumOpcode::I64ExtendI32U, vec![value]),
            (Flat::I64, Flat::I32) => self.num(ctx, NumOpcode::I32WrapI64, vec![value]),
            _ => value,
        };
        match to {
            Flat::F32 => self.num(ctx, NumOpcode::F32ReinterpretI32, vec![value]),
            Flat::F64 => self.num(ctx, NumOpcode::F64ReinterpretI64, vec![value]),
            _ => value,
        }
    }

    pub(super) fn lift_sum(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        mut source: Canonical<'_>,
    ) -> Result<Value> {
        let tag = source.read(self, ctx, ty.discriminant_kind(), 0)?;
        let flat = if matches!(source, Canonical::Memory(..)) {
            None
        } else {
            Some(
                ty.sum_flat()
                    .into_iter()
                    .skip(1)
                    .map(|slot| {
                        source
                            .read(self, ctx, slot.mem_kind(), 0)
                            .map(|value| (value, slot))
                    })
                    .collect::<Result<Vec<_>>>()?,
            )
        };
        let result = wasm_type(ctx, mir)?;
        Ok(
            self.sum_switch(ctx, tag, ty.cases(), vec![result], |b, ctx, case, abi| {
                let payload = if let Some(abi) = abi {
                    let payload_mir = b.native_payload(ctx, ty, mir, case, abi)?.mir;
                    let lift_mir = scalar_mir_type(ctx, payload_mir);
                    let mut index = 0;
                    let source = match &flat {
                        Some(values) => Canonical::JoinedInput(values, &mut index),
                        None => source.at(ty.payload_offset()),
                    };
                    Some((payload_mir, b.lift_abi(ctx, abi, lift_mir, source)?))
                } else {
                    None
                };
                Ok(vec![b.lift_case(ctx, ty, mir, case, payload)?])
            })?[0],
        )
    }

    fn sum_tag(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        value: Carriers<Value>,
    ) -> Result<Value> {
        Ok(match ty {
            AbiType::Absent { null, .. } => {
                let missing = self.is_absent(ctx, value, mir)?;
                let present = self.i32_const(ctx, if *null { 2 } else { 1 });
                let absent = if *null {
                    let is_null = self.absence_test(ctx, value, mir, true)?;
                    let zero = self.i32_const(ctx, 0);
                    let one = self.i32_const(ctx, 1);
                    self.select(ctx, is_null, zero, one)
                } else {
                    self.i32_const(ctx, 0)
                };
                self.select(ctx, missing, absent, present)
            }
            AbiType::Option(_) => {
                ensure!(
                    optional_value(ctx, mir).is_some(),
                    "WIT option needs a nullable TypeScript value"
                );
                let absent = self.is_absent(ctx, value, mir)?;
                self.num(ctx, NumOpcode::I32Eqz, vec![absent])
            }
            AbiType::Result { .. } => {
                let value = value.scalar()?;
                let record = self.record_type(ctx, mir)?;
                let (offset, field) = Self::record_type_field_slot(ctx, record, "ok")?;
                ensure!(
                    repr(ctx, field)? == Repr::Bool,
                    "result ok field must be boolean"
                );
                let ok = self.load(ctx, MemKind::I32, value, offset);
                self.num(ctx, NumOpcode::I32Eqz, vec![ok])
            }
            AbiType::Variant(cases) => {
                let value = value.scalar()?;
                ensure!(
                    mir.deref(ctx).is::<perry_mir::VariantType>(),
                    "variant needs a closed tagged union"
                );
                let tag = self.load(ctx, MemKind::I32, value, 0);
                self.string_tag(ctx, cases.iter().map(|(name, _)| name.as_str()), tag)
            }
            AbiType::Enum(names) => {
                let value = value.scalar()?;
                ensure!(
                    mir.deref(ctx).is::<perry_mir::StringType>(),
                    "WIT enum needs a string literal union"
                );
                self.string_tag(ctx, names.iter().map(String::as_str), value)
            }
            other => bail!("WIT {other:?} is not a sum type"),
        })
    }

    /// Index of the first of `names` equal to the string `value`, or -1.
    fn string_tag<'n>(
        &mut self,
        ctx: &mut Context,
        names: impl Iterator<Item = &'n str>,
        value: Value,
    ) -> Value {
        let mut tag = self.i32_const(ctx, -1);
        for (index, name) in names.enumerate() {
            let text = self.state.data.string(name);
            let text = self.i32_const(ctx, text as i32);
            let comparison = self.runtime(ctx, Runtime::StringCompare, vec![value, text]);
            let matches = self.num(ctx, NumOpcode::I32Eqz, vec![comparison]);
            let index = self.i32_const(ctx, index as i32);
            tag = self.select(ctx, matches, index, tag);
        }
        tag
    }

    fn read_native_payload(
        &mut self,
        ctx: &mut Context,
        payload: NativePayload,
        value: Carriers<Value>,
    ) -> Result<(TypeHandle, Carriers<Value>)> {
        let Some((offset, field)) = payload.slot else {
            return Ok((payload.mir, value));
        };
        let value = self.load_slot_value(ctx, value.scalar()?, offset, field)?;
        if payload.mir != field {
            let absent = self.is_absent(ctx, value, field)?;
            let trap = self.new_block(ctx, vec![]);
            let present = self.new_block(ctx, vec![]);
            self.cond_br(ctx, absent, (trap, vec![]), (present, vec![]));
            self.block = trap;
            self.trap(ctx);
            self.block = present;
        }
        Ok((payload.mir, value))
    }

    pub(super) fn lower_sum(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        source: BoundaryValue,
        mut target: Canonical<'_>,
    ) -> Result<()> {
        let layout = if matches!(target, Canonical::Memory(..)) {
            Vec::new()
        } else {
            ty.sum_flat()
        };
        let write_case = |b: &mut Self, ctx: &mut Context, case, payload_abi: Option<&AbiType>| {
            let mut out = Vec::new();
            let mut destination = match target {
                Canonical::Memory(address, offset) => Canonical::Memory(address, offset),
                _ => Canonical::Flat(&mut out),
            };
            let tag = b.i32_const(ctx, case as i32);
            destination.write(b, ctx, ty.discriminant_kind(), 0, tag)?;
            if let Some(payload_abi) = payload_abi {
                let (mir, value) = match source {
                    BoundaryValue::Completion { payload, .. } => payload
                        .ok_or_else(|| anyhow!("WIT result payload requires a return value"))?,
                    BoundaryValue::Native(mir, value) => {
                        let payload = b.native_payload(ctx, ty, mir, case, payload_abi)?;
                        b.read_native_payload(ctx, payload, value)?
                    }
                };
                let destination = match destination {
                    Canonical::Memory(address, offset) => {
                        Canonical::Memory(address, offset + ty.payload_offset())
                    }
                    _ => Canonical::JoinedOutput(&mut out, &layout),
                };
                b.lower_abi(ctx, payload_abi, mir, value, destination)?;
            }
            for slot in &layout[out.len()..] {
                let zero = b.i64_const(ctx, 0);
                out.push(b.coerce_flat(ctx, zero, Flat::I64, *slot));
            }
            Ok(out)
        };
        let out = match source {
            BoundaryValue::Completion { case, .. } => {
                write_case(self, ctx, case, ty.cases().nth(case).flatten())?
            }
            BoundaryValue::Native(mir, value) => {
                let tag = self.sum_tag(ctx, ty, mir, value)?;
                let results = layout.iter().map(|ty| flat_type(ctx, *ty)).collect();
                self.sum_switch(ctx, tag, ty.cases(), results, write_case)?
            }
        };
        match target {
            Canonical::Flat(values) if values.is_empty() => *values = out,
            Canonical::Flat(values) => values.extend(out),
            _ => {
                for (value, slot) in out.into_iter().zip(layout) {
                    target.write(self, ctx, slot.mem_kind(), 0, value)?;
                }
            }
        }
        Ok(())
    }
}
