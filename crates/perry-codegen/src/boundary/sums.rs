//! Canonical discriminants and payloads for closed WIT sums.
use super::*;
use perry_mir::{absence, optional_value, present_type};
use perry_wit_diagnostics::Diagnostic;

impl AbiType {
    pub(crate) fn accepts_exception(&self) -> bool {
        match &self.kind {
            AbiKind::Result { err, .. } => err.as_deref().is_none_or(|err| match &err.kind {
                _ if err.is_scalar() => true,
                AbiKind::String | AbiKind::Enum(_, _) => true,
                AbiKind::Record(fields) => {
                    fields.len() == 2
                        && ["name", "message"].iter().all(|name| {
                            fields
                                .iter()
                                .any(|(field, ty)| field == name && ty.kind == AbiKind::String)
                        })
                }
                _ => false,
            }),
            AbiKind::OkHandle { .. } => true,
            _ => false,
        }
    }
    pub(super) fn is_sum(&self) -> bool {
        matches!(
            self.kind,
            AbiKind::Option(_)
                | AbiKind::Result { .. }
                | AbiKind::Enum(_, _)
                | AbiKind::Variant(_, _)
        )
    }

    fn case(&self, index: usize) -> Option<(&str, Option<&Self>)> {
        match &self.kind {
            AbiKind::Variant(_, cases) => cases
                .get(index)
                .map(|(name, payload)| (name.as_str(), payload.as_ref())),
            AbiKind::Enum(_, names) => names.get(index).map(|name| (name.as_str(), None)),
            AbiKind::Option(inner) => [("none", None), ("some", Some(inner.as_ref()))]
                .get(index)
                .copied(),
            AbiKind::Result { ok, err } => [("ok", ok.as_deref()), ("err", err.as_deref())]
                .get(index)
                .copied(),
            _ => None,
        }
    }

    pub(super) fn cases(&self) -> impl Iterator<Item = (&str, Option<&Self>)> {
        (0..).map_while(|index| self.case(index))
    }

    /// How the case index is stored in memory.
    pub(super) fn discriminant_kind(&self) -> MemKind {
        use wit_parser::Int;
        match &self.kind {
            AbiKind::Enum(tag, _) | AbiKind::Variant(tag, _) => match tag {
                Int::U8 => MemKind::I32U8,
                Int::U16 => MemKind::I32U16,
                Int::U32 => MemKind::I32,
                Int::U64 => unreachable!("WIT discriminants fit in 32 bits"),
            },
            AbiKind::Option(_) | AbiKind::Result { .. } => MemKind::I32U8,
            _ => unreachable!("expected a sum"),
        }
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
        let AbiKind::Result { err, .. } = &result.kind else {
            bail_internal!("completion requires a WIT result");
        };
        let Some(err) = err.as_deref() else {
            return Ok(None);
        };
        if let AbiKind::Record(fields) = &err.kind {
            ensure_diag!(
                fields.len() == 2
                    && ["name", "message"].iter().all(|name| fields
                        .iter()
                        .any(|(field, ty)| field == name && ty.kind == AbiKind::String)),
                codes::CONTRACT_MISMATCH,
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
            let record = RecordType::get(
                ctx,
                identifier("perry_completion_error"),
                Some(vec![("message".into(), string), ("name".into(), string)]),
            )
            .map_err(|_| Diagnostic::internal("inconsistent completion Error record shape"))?;
            let name = self.runtime(ctx, Runtime::StdExceptionName, vec![exception]);
            let message = self.runtime(ctx, Runtime::ExceptionMessage, vec![exception]);
            let layout = self.state.record_layouts.get(ctx, record)?;
            let fields = [("name", name), ("message", message)]
                .into_iter()
                .map(|(key, value)| Ok((layout.field(ctx, key)?, value)))
                .collect::<Result<_>>()?;
            let value = self.build_record(ctx, &layout, fields)?;
            return Ok(Some((record.into(), Carriers::Scalar(value))));
        }
        let (expected, mir, kind) = match &err.kind {
            AbiKind::String | AbiKind::Enum(_, _) => (
                exception_layout::TAG_STRING,
                perry_mir::StringType::get(ctx).into(),
                MemKind::I32,
            ),
            AbiKind::Scalar(Wit::S64 | Wit::U64) => (
                exception_layout::TAG_BIGINT,
                perry_mir::BigIntType::get(ctx).into(),
                MemKind::I32,
            ),
            AbiKind::Scalar(Wit::Bool) => (
                exception_layout::TAG_BOOLEAN,
                IntegerType::get(ctx, 1, Signedness::Signless).into(),
                MemKind::I32,
            ),
            _ if err.is_scalar() => (
                exception_layout::TAG_NUMBER,
                FP64Type::get(ctx).into(),
                MemKind::F64,
            ),
            _ => bail_diag!(
                codes::CONTRACT_MISMATCH,
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
    pub(super) fn sum_switch<T>(
        &mut self,
        ctx: &mut Context,
        tag: Value,
        cases: impl Iterator<Item = T>,
        results: Vec<TypeHandle>,
        mut case: impl FnMut(&mut Self, &mut Context, usize, T) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let join = self.new_block(ctx, results);
        for (index, payload) in cases.enumerate() {
            let discriminant = self.i32_const(ctx, index as i32);
            let matches = self.num(ctx, NumOpcode::I32Eq, vec![tag, discriminant]);
            let next = self.enter_if(ctx, matches);
            let values = case(self, ctx, index, payload)?;
            self.br(ctx, join, values);
            self.block = next;
        }
        self.trap(ctx);
        self.block = join;
        Ok(join.deref(ctx).arguments().collect())
    }

    fn native_payload(
        &mut self,
        ctx: &Context,
        ty: &AbiType,
        mir: TypeHandle,
        (case, name): (usize, &str),
        abi: &AbiType,
    ) -> Result<NativePayload> {
        let (offset, field) = match &ty.kind {
            AbiKind::Option(_) => {
                return Ok(NativePayload {
                    mir: optional_value(ctx, mir).ok_or_else(|| {
                        Diagnostic::new(
                            codes::CONTRACT_MISMATCH,
                            "WIT option needs a nullable TypeScript value",
                        )
                    })?,
                    slot: None,
                });
            }
            AbiKind::Result { .. } if perry_mir::typed::<RecordType>(ctx, mir).is_some() => {
                let layout = self.boundary_record(ctx, mir)?;
                let field = layout
                    .result_fields()?
                    .payloads
                    .get(case)
                    .copied()
                    .flatten()
                    .ok_or_else(|| diag!(codes::CONTRACT_MISMATCH, "result payload is missing"))?;
                return Ok(NativePayload {
                    mir: if matches!(abi.kind, AbiKind::Option(_)) {
                        field.ty
                    } else {
                        present_type(ctx, field.ty)
                    },
                    slot: Some((field.offset, field.ty)),
                });
            }
            AbiKind::Variant(_, _) | AbiKind::Result { .. } => {
                let variant = TypedHandle::<perry_mir::VariantType>::from_handle(mir, ctx)
                    .map_err(|_| {
                        Diagnostic::new(
                            codes::CONTRACT_MISMATCH,
                            "variant needs a closed tagged union",
                        )
                    })?;
                let field = variant
                    .deref(ctx)
                    .cases()
                    .iter()
                    .find(|(tag, _)| tag == name)
                    .and_then(|(_, payload)| *payload)
                    .ok_or_else(|| {
                        Diagnostic::new(codes::CONTRACT_MISMATCH, "variant payload is missing")
                    })?;
                (perry_abi::layout::variant::VALUE, field)
            }
            _ => bail_internal!("sum has no payload"),
        };
        Ok(NativePayload {
            mir: field,
            slot: Some((offset, field)),
        })
    }

    fn lift_case(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        (case, name): (usize, &str),
        payload: Option<(TypeHandle, Value)>,
    ) -> Result<Value> {
        match &ty.kind {
            AbiKind::Variant(_, _) | AbiKind::Result { .. }
                if mir.deref(ctx).is::<perry_mir::VariantType>() =>
            {
                self.variant_new(ctx, name, payload)
            }
            AbiKind::Enum(..) => {
                ensure_diag!(
                    mir.deref(ctx).is::<perry_mir::StringType>(),
                    codes::CONTRACT_MISMATCH,
                    "WIT enum needs a string literal union"
                );
                let text = self.state.data.string(name);
                Ok(self.i32_const(ctx, text as i32))
            }
            AbiKind::Option(_) => {
                ensure_diag!(
                    optional_value(ctx, mir).is_some(),
                    codes::CONTRACT_MISMATCH,
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
            AbiKind::Result { .. } => {
                let layout = self.boundary_record(ctx, mir)?;
                let fields = layout.result_fields()?;
                let mut values = Vec::with_capacity(layout.fields.len());
                for (name, field) in &layout.fields {
                    let value = if field.index == fields.tag.index {
                        self.i32_const(ctx, i32::from(case == 0))
                    } else if fields
                        .payloads
                        .get(case)
                        .copied()
                        .flatten()
                        .is_some_and(|p| p.index == field.index)
                    {
                        payload
                            .ok_or_else(|| {
                                diag!(
                                    codes::CONTRACT_MISMATCH,
                                    "payload-free result has an unexpected {name} field"
                                )
                            })?
                            .1
                    } else {
                        ensure_diag!(
                            absence(ctx, field.ty).is_some_and(|a| a.admits_undefined()),
                            codes::CONTRACT_MISMATCH,
                            "inactive result field {name} must admit undefined"
                        );
                        let field_lift = scalar_mir_type(ctx, field.ty);
                        self.undefined(ctx, field_lift)?
                    };
                    values.push((*field, value));
                }
                self.build_record(ctx, &layout, values)
            }
            other => bail_internal!("WIT {other:?} is not a sum type"),
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
                ty.direct()?
                    .iter()
                    .copied()
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
        Ok(self.sum_switch(
            ctx,
            tag,
            ty.cases(),
            vec![result],
            |b, ctx, case, (name, abi)| {
                let payload = if let Some(abi) = abi {
                    let payload_mir = b.native_payload(ctx, ty, mir, (case, name), abi)?.mir;
                    let lift_mir = scalar_mir_type(ctx, payload_mir);
                    let mut index = 0;
                    let source = match &flat {
                        Some(values) => Canonical::JoinedInput(values, &mut index),
                        None => source.at(abi.offset),
                    };
                    Some((payload_mir, b.lift_abi(ctx, abi, lift_mir, source)?))
                } else {
                    None
                };
                Ok(vec![b.lift_case(ctx, ty, mir, (case, name), payload)?])
            },
        )?[0])
    }

    fn sum_tag(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        value: Carriers<Value>,
    ) -> Result<Value> {
        Ok(match &ty.kind {
            AbiKind::Option(_) => {
                ensure_diag!(
                    optional_value(ctx, mir).is_some(),
                    codes::CONTRACT_MISMATCH,
                    "WIT option needs a nullable TypeScript value"
                );
                let absent = self.is_absent(ctx, value, mir)?;
                self.num(ctx, NumOpcode::I32Eqz, vec![absent])
            }
            AbiKind::Result { .. } if mir.deref(ctx).is::<RecordType>() => {
                let value = value.scalar()?;
                let layout = self.boundary_record(ctx, mir)?;
                let field = layout.result_fields()?.tag;
                let ok = self.load(ctx, MemKind::I32, value, field.offset);
                self.num(ctx, NumOpcode::I32Eqz, vec![ok])
            }
            AbiKind::Variant(_, _) | AbiKind::Result { .. } => {
                let value = value.scalar()?;
                ensure_diag!(
                    mir.deref(ctx).is::<perry_mir::VariantType>(),
                    codes::CONTRACT_MISMATCH,
                    "variant needs a closed tagged union"
                );
                let tag = self.load(ctx, MemKind::I32, value, 0);
                self.string_tag(ctx, ty.cases().map(|(name, _)| name), tag)
            }
            AbiKind::Enum(_, names) => {
                let value = value.scalar()?;
                ensure_diag!(
                    mir.deref(ctx).is::<perry_mir::StringType>(),
                    codes::CONTRACT_MISMATCH,
                    "WIT enum needs a string literal union"
                );
                self.string_tag(ctx, names.iter().map(String::as_str), value)
            }
            other => bail_internal!("WIT {other:?} is not a sum type"),
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
            self.trap_if(ctx, absent);
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
            &[][..]
        } else {
            ty.direct()?
        };
        let write_case = |b: &mut Self,
                          ctx: &mut Context,
                          case,
                          (name, payload_abi): (&str, Option<&AbiType>)| {
            let mut out = Vec::new();
            let mut destination = match target {
                Canonical::Memory(address, offset) => Canonical::Memory(address, offset),
                _ => Canonical::Flat(&mut out),
            };
            let tag = b.i32_const(ctx, case as i32);
            destination.write(b, ctx, ty.discriminant_kind(), 0, tag)?;
            if let Some(payload_abi) = payload_abi {
                let (mir, value) = match source {
                    BoundaryValue::Completion { payload, .. } => payload.ok_or_else(|| {
                        Diagnostic::new(
                            codes::CONTRACT_MISMATCH,
                            "WIT result payload requires a return value",
                        )
                    })?,
                    BoundaryValue::Native(mir, value) => {
                        let payload = b.native_payload(ctx, ty, mir, (case, name), payload_abi)?;
                        b.read_native_payload(ctx, payload, value)?
                    }
                };
                let destination = match destination {
                    Canonical::Memory(address, offset) => {
                        Canonical::Memory(address, offset + payload_abi.offset)
                    }
                    _ => Canonical::JoinedOutput(&mut out, layout),
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
            BoundaryValue::Completion { case, .. } => write_case(
                self,
                ctx,
                case,
                ty.case(case)
                    .ok_or_else(|| internal!("missing WIT completion case"))?,
            )?,
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
