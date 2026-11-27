//! Canonical discriminants and payloads for closed WIT sums.
use super::*;
use perry_mir::{absence, optional_value, present_type};

type AbiCase = (String, Option<AbiField>);

#[derive(Clone, Debug, PartialEq)]
pub enum SumLayout {
    Option(AbiField),
    Result([Option<AbiField>; 2]),
    Enum(wit_parser::Int, Box<[String]>),
    Variant(wit_parser::Int, Box<[AbiCase]>),
}

impl SumLayout {
    pub(super) fn cases(&self) -> impl Iterator<Item = (&str, Option<&AbiField>)> {
        (0..).map_while(move |index| match (self, index) {
            (Self::Option(_), 0) => Some(("none", None)),
            (Self::Option(ty), 1) => Some(("some", Some(ty))),
            (Self::Result([ok, _]), 0) => Some(("ok", ok.as_ref())),
            (Self::Result([_, err]), 1) => Some(("err", err.as_ref())),
            (Self::Enum(_, names), i) => names.get(i).map(|name| (name.as_str(), None)),
            (Self::Variant(_, cases), i) => {
                cases.get(i).map(|(name, ty)| (name.as_str(), ty.as_ref()))
            }
            _ => None,
        })
    }

    pub(super) fn discriminant(&self) -> Result<MemKind> {
        use wit_parser::Int;
        let tag = match self {
            Self::Option(_) | Self::Result(_) => Int::U8,
            Self::Variant(tag, _) | Self::Enum(tag, _) => *tag,
        };
        Ok(match tag {
            Int::U8 => MemKind::I32U8,
            Int::U16 => MemKind::I32U16,
            Int::U32 => MemKind::I32,
            Int::U64 => return Err(internal!("WIT discriminant exceeds 32 bits")),
        })
    }
}

impl AbiType {
    fn is_error_record(&self) -> bool {
        let AbiKind::Record(record) = &self.kind else {
            return false;
        };
        let fields = &record.fields;
        fields.len() == 2
            && ["name", "message"].iter().all(|name| {
                fields
                    .iter()
                    .any(|(field, ty)| field == name && ty.ty.kind == AbiKind::String)
            })
    }

    pub(super) fn result_payloads(&self) -> Option<[Option<&Self>; 2]> {
        let AbiKind::Sum(SumLayout::Result(cases)) = &self.kind else {
            return None;
        };
        Some(
            cases
                .each_ref()
                .map(|case| case.as_ref().map(|field| field.ty.as_ref())),
        )
    }

    pub(crate) fn accepts_exception(&self) -> bool {
        self.result_payloads().is_some_and(|[_, err]| {
            err.is_none_or(|err| {
                err.is_scalar()
                    || matches!(
                        err.kind,
                        AbiKind::String | AbiKind::Sum(SumLayout::Enum(..))
                    )
                    || err.is_error_record()
            })
        })
    }
    fn payload_slots(&self) -> Result<&[WasmType]> {
        self.direct()?
            .get(1..)
            .ok_or_else(|| internal!("sum layout has no discriminant"))
    }
}

#[derive(Clone, Copy)]
pub(super) enum BoundaryValue<T = TypeHandle> {
    Native(T, Carriers<Value>),
    Case(
        &'static [&'static str],
        Option<(TypeHandle, Carriers<Value>)>,
    ),
}

struct NativePayload {
    mir: TypeHandle,
    slot: Option<(u32, TypeHandle)>,
}

pub(super) struct TaggedResult<'a> {
    pub(super) layout: &'a RecordLayout,
    tag: RecordField,
}

impl<'a> TaggedResult<'a> {
    pub(super) fn of(ctx: &Context, layout: &'a RecordLayout) -> Option<Self> {
        let tag = *layout.fields.get(perry_mir::RESULT_TAG)?;
        (repr(ctx, tag.ty).ok()? == Repr::Bool).then_some(Self { tag, layout })
    }
}

enum NativeSum<'a> {
    Optional { mir: TypeHandle, inner: TypeHandle },
    Enum,
    Variant(TypedHandle<perry_mir::VariantType>),
    Result(TaggedResult<'a>),
}

impl NativeSum<'_> {
    fn payload(
        &self,
        ctx: &Context,
        case: usize,
        name: &str,
        abi: &AbiType,
    ) -> Result<NativePayload> {
        let (mir, slot) = match self {
            Self::Optional { inner, .. } => (*inner, None),
            Self::Result(layout) => {
                let field = perry_mir::RESULT_PAYLOADS
                    .get(case)
                    .and_then(|key| layout.layout.fields.get(*key))
                    .ok_or_else(|| diag!(codes::CONTRACT_MISMATCH, "result payload is missing"))?;
                let mir = if matches!(abi.kind, AbiKind::Sum(SumLayout::Option(_))) {
                    field.ty
                } else {
                    present_type(ctx, field.ty)
                };
                (mir, Some((field.offset, field.ty)))
            }
            Self::Variant(variant) => {
                let field =
                    variant.deref(ctx).case(name).flatten().ok_or_else(|| {
                        diag!(codes::CONTRACT_MISMATCH, "variant payload is missing")
                    })?;
                (field, Some((perry_abi::layout::variant::VALUE, field)))
            }
            Self::Enum => bail_internal!("enum has no payload"),
        };
        Ok(NativePayload { mir, slot })
    }
}

impl<'e> FunctionLowering<'_, 'e> {
    /// Thrown scalars are checked against the declared WIT error payload before lowering.
    pub(super) fn completion_error(
        &mut self,
        ctx: &mut Context,
        result: &AbiType,
        exception: Value,
    ) -> Result<Option<(TypeHandle, Carriers<Value>)>> {
        use perry_abi::layout::exception as exception_layout;
        let Some([_, err]) = result.result_payloads() else {
            bail_internal!("completion requires a WIT result");
        };
        let Some(err) = err else {
            return Ok(None);
        };
        if matches!(err.kind, AbiKind::Record(_)) {
            ensure_diag!(
                err.is_error_record(),
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
            let record = self.state.record_layouts.completion_error;
            let name = self.runtime(ctx, Runtime::StdExceptionName, vec![exception]);
            let message = self.runtime(ctx, Runtime::ExceptionMessage, vec![exception]);
            let layout = self.state.record_layouts.get(ctx, record)?;
            let fields = [("name", name), ("message", message)]
                .into_iter()
                .map(|(key, value)| Ok((layout.field(ctx, key)?, value)))
                .collect::<Result<_>>()?;
            let value = self.build_record(ctx, layout, fields)?;
            return Ok(Some((record.into(), Carriers::Scalar(value))));
        }
        let (expected, mir, kind) = match &err.kind {
            AbiKind::String | AbiKind::Sum(SumLayout::Enum(..)) => (
                exception_layout::TAG_STRING,
                perry_mir::StringType::get(ctx).into(),
                MemKind::I32,
            ),
            AbiKind::Scalar(Scalar::Integer(Integer::I64 | Integer::U64)) => (
                exception_layout::TAG_BIGINT,
                perry_mir::BigIntType::get(ctx).into(),
                MemKind::I32,
            ),
            AbiKind::Scalar(Scalar::Bool) => (
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

    fn native_sum(
        &mut self,
        ctx: &Context,
        wit: &SumLayout,
        mir: TypeHandle,
    ) -> Result<NativeSum<'e>> {
        Ok(match wit {
            SumLayout::Option(_) => NativeSum::Optional {
                mir,
                inner: optional_value(ctx, mir).ok_or_else(|| {
                    diag!(
                        codes::CONTRACT_MISMATCH,
                        "WIT option needs a nullable TypeScript value"
                    )
                })?,
            },
            SumLayout::Enum(..) => {
                ensure_diag!(
                    mir.deref(ctx).is::<perry_mir::StringType>(),
                    codes::CONTRACT_MISMATCH,
                    "WIT enum needs a string literal union"
                );
                NativeSum::Enum
            }
            SumLayout::Result(_) if mir.deref(ctx).is::<RecordType>() => NativeSum::Result(
                TaggedResult::of(ctx, self.state.record_layouts.boundary_record(ctx, mir)?)
                    .ok_or_else(|| {
                        diag!(codes::CONTRACT_MISMATCH, "result ok field must be boolean")
                    })?,
            ),
            SumLayout::Variant(..) | SumLayout::Result(_) => {
                NativeSum::Variant(perry_mir::typed(ctx, mir).ok_or_else(|| {
                    diag!(
                        codes::CONTRACT_MISMATCH,
                        "variant needs a closed tagged union"
                    )
                })?)
            }
        })
    }

    fn lift_case(
        &mut self,
        ctx: &mut Context,
        sum: &NativeSum<'_>,
        (case, name): (usize, &str),
        payload: Option<(TypeHandle, Value)>,
    ) -> Result<Value> {
        match sum {
            NativeSum::Variant(_) => self.variant_new(ctx, name, payload),
            NativeSum::Enum => {
                let text = self.state.data.string(name);
                Ok(self.i32_const(ctx, text as i32))
            }
            NativeSum::Optional { mir, .. } => {
                let mir = *mir;
                match payload {
                    Some((_, value)) => Ok(value),
                    None if absence(ctx, mir).is_some_and(|a| a.admits_null()) => {
                        self.null(ctx, mir)
                    }
                    None => self.undefined(ctx, mir),
                }
            }
            NativeSum::Result(fields) => {
                let layout = fields.layout;
                let mut values = Vec::with_capacity(layout.fields.len());
                for (name, field) in &layout.fields {
                    let value = if field.index == fields.tag.index {
                        self.i32_const(ctx, i32::from(case == 0))
                    } else if perry_mir::RESULT_PAYLOADS.get(case) == Some(&name.as_str()) {
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
                self.build_record(ctx, layout, values)
            }
        }
    }

    fn coerce_slots(
        &mut self,
        ctx: &mut Context,
        values: &mut [Value],
        to: &[WasmType],
    ) -> Result<()> {
        ensure_internal!(values.len() <= to.len(), "invalid joined payload layout");
        for (value, &ty) in values.iter_mut().zip(to) {
            *value = self.repack_bits(ctx, *value, flat_type(ctx, ty));
        }
        Ok(())
    }

    pub(super) fn lift_sum(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        wit: &SumLayout,
        mir: TypeHandle,
        mut source: Canonical<&mut &[Value]>,
    ) -> Result<Value> {
        let sum = self.native_sum(ctx, wit, mir)?;
        let tag = source.read(self, ctx, wit.discriminant()?, 0)?;
        let flat = match &mut source {
            Canonical::Flat(values) => {
                let layout = ty.payload_slots()?;
                let values = values
                    .split_off(..layout.len())
                    .ok_or_else(|| internal!("too few joined flat values"))?;
                Some(values)
            }
            Canonical::Memory(..) => None,
        };
        let mut slots = Vec::new();
        let result = wasm_type(ctx, mir)?;
        Ok(self.sum_switch(
            ctx,
            tag,
            wit.cases(),
            vec![result],
            |b, ctx, case, (name, abi)| {
                let payload = if let Some(abi) = abi {
                    let payload_mir = sum.payload(ctx, case, name, &abi.ty)?.mir;
                    let lift_mir = scalar_mir_type(ctx, payload_mir);
                    let value = match flat {
                        Some(values) => {
                            let layout = abi.ty.direct()?;
                            slots.clear();
                            slots.extend_from_slice(
                                values
                                    .get(..layout.len())
                                    .ok_or_else(|| internal!("too few joined flat values"))?,
                            );
                            b.coerce_slots(ctx, &mut slots, layout)?;
                            b.lift_abi(
                                ctx,
                                &abi.ty,
                                lift_mir,
                                Canonical::Flat(&mut slots.as_slice()),
                            )?
                        }
                        None => b.lift_abi(ctx, &abi.ty, lift_mir, source.at(abi.offset))?,
                    };
                    Some((payload_mir, value))
                } else {
                    None
                };
                Ok(vec![b.lift_case(ctx, &sum, (case, name), payload)?])
            },
        )?[0])
    }

    fn sum_tag(
        &mut self,
        ctx: &mut Context,
        wit: &SumLayout,
        sum: &NativeSum<'_>,
        value: Carriers<Value>,
    ) -> Result<Value> {
        let names = wit.cases().map(|(name, _)| name);
        Ok(match sum {
            NativeSum::Optional { mir, .. } => {
                let absent = self.is_absent(ctx, value, *mir)?;
                self.num(ctx, NumOpcode::I32Eqz, vec![absent])
            }
            NativeSum::Result(layout) => {
                let field = layout.tag;
                let ok = self.load(ctx, MemKind::I32, value.scalar()?, field.offset);
                self.num(ctx, NumOpcode::I32Eqz, vec![ok])
            }
            NativeSum::Variant(_) => {
                let tag = self.load(ctx, MemKind::I32, value.scalar()?, 0);
                self.string_tag(ctx, names, tag)
            }
            NativeSum::Enum => self.string_tag(ctx, names, value.scalar()?),
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
        wit: &SumLayout,
        source: BoundaryValue,
        target: Canonical<&mut Vec<Value>>,
    ) -> Result<()> {
        let layout = if matches!(target, Canonical::Memory(..)) {
            &[][..]
        } else {
            ty.direct()?
        };
        let source = match source {
            BoundaryValue::Native(mir, value) => {
                BoundaryValue::Native(self.native_sum(ctx, wit, mir)?, value)
            }
            BoundaryValue::Case(path, payload) => BoundaryValue::Case(path, payload),
        };
        let write_case = |b: &mut Self,
                          ctx: &mut Context,
                          case,
                          (name, payload_abi): (&str, Option<&AbiField>)| {
            let mut out = Vec::new();
            let mut destination = match target {
                Canonical::Memory(address, offset) => Canonical::Memory(address, offset),
                _ => Canonical::Flat(&mut out),
            };
            let tag = b.i32_const(ctx, case as i32);
            destination.write(b, ctx, wit.discriminant()?, 0, tag);
            if let Some(payload_abi) = payload_abi {
                let payload = match source {
                    BoundaryValue::Case([_, next @ ..], payload) if !next.is_empty() => {
                        BoundaryValue::Case(next, payload)
                    }
                    BoundaryValue::Case(_, Some((mir, value))) => BoundaryValue::Native(mir, value),
                    BoundaryValue::Case(_, None) => bail_diag!(
                        codes::CONTRACT_MISMATCH,
                        "WIT result payload requires a return value"
                    ),
                    BoundaryValue::Native(ref sum, value) => {
                        let payload = sum.payload(ctx, case, name, &payload_abi.ty)?;
                        let (mir, value) = b.read_native_payload(ctx, payload, value)?;
                        BoundaryValue::Native(mir, value)
                    }
                };
                b.lower_abi(
                    ctx,
                    &payload_abi.ty,
                    payload,
                    destination.at(payload_abi.offset),
                )?;
                if let Canonical::Flat(values) = destination {
                    b.coerce_slots(ctx, &mut values[1..], ty.payload_slots()?)?;
                }
            }
            for slot in layout
                .get(out.len()..)
                .ok_or_else(|| internal!("sum exceeds flat layout"))?
            {
                out.push(b.zero(ctx, flat_type(ctx, *slot)));
            }
            Ok(out)
        };
        let out = match source {
            BoundaryValue::Case(path, _) => {
                let (case, item) = wit
                    .cases()
                    .enumerate()
                    .find(|(_, (name, _))| Some(name) == path.first())
                    .ok_or_else(|| internal!("missing WIT completion case {path:?}"))?;
                ensure_internal!(
                    path.len() == 1 || item.1.is_some(),
                    "WIT case {path:?} continues past an empty case"
                );
                write_case(self, ctx, case, item)?
            }
            BoundaryValue::Native(ref sum, value) => {
                let tag = self.sum_tag(ctx, wit, sum, value)?;
                let results = layout.iter().map(|ty| flat_type(ctx, *ty)).collect();
                self.sum_switch(ctx, tag, wit.cases(), results, write_case)?
            }
        };
        if let Canonical::Flat(values) = target {
            values.extend(out);
        }
        Ok(())
    }
}
