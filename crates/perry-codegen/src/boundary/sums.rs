//! Canonical discriminants and payloads for closed WIT sums.
use super::*;
use perry_mir::optional::{absence, optional_value};

/// A WIT `result` that a source function fills completion-style: a compatible return is
/// `ok`, a checked throw is `err`.
#[derive(Clone, Copy)]
pub(super) struct Completion<'a> {
    pub(super) result: &'a AbiType,
    err: Option<&'a AbiType>,
}

impl AbiType {
    /// An explicit tagged return wins over a compatible success-payload shape.
    /// This result as a completion-style mapping of a source function returning `mir`,
    /// unless the source returns an explicit tagged result.
    pub(super) fn completion(
        &self,
        ctx: &Context,
        mir: Option<TypeHandle>,
    ) -> Option<Completion<'_>> {
        let Self::Result { ok, err } = self else {
            return None;
        };
        let completion = Completion {
            result: self,
            err: err.as_deref(),
        };
        let Some(record) = mir.and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
        else {
            return Some(completion);
        };
        let tagged = record_field(ctx, record, "ok")
            .is_ok_and(|(_, ty)| crate::repr(ctx, ty).is_ok_and(|r| r == crate::Repr::Bool))
            && ok
                .as_ref()
                .is_none_or(|_| record_field(ctx, record, "value").is_ok())
            && err
                .as_ref()
                .is_none_or(|_| record_field(ctx, record, "error").is_ok());
        (!tagged).then_some(completion)
    }
    pub(super) fn is_sum(&self) -> bool {
        matches!(
            self,
            Self::Option(_) | Self::Result { .. } | Self::Enum(_) | Self::Variant(_)
        )
    }

    pub(super) fn cases(&self) -> Vec<Option<&Self>> {
        match self {
            Self::Variant(cases) => cases.iter().map(|(_, payload)| payload.as_ref()).collect(),
            Self::Option(inner) => vec![None, Some(inner)],
            Self::Result { ok, err } => vec![ok.as_deref(), err.as_deref()],
            Self::Enum(names) => vec![None; names.len()],
            _ => unreachable!("not a sum"),
        }
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
        for case in self.cases().into_iter().flatten() {
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
            .into_iter()
            .flatten()
            .map(Self::alignment)
            .fold(self.discriminant().alignment(), u32::max)
    }

    pub(super) fn payload_offset(&self) -> u32 {
        align(self.discriminant().size(), self.sum_alignment())
    }

    pub(super) fn sum_size(&self) -> u32 {
        let payload = self
            .cases()
            .into_iter()
            .flatten()
            .map(Self::size)
            .max()
            .unwrap_or(0);
        align(self.payload_offset() + payload, self.sum_alignment())
    }
}

impl FunctionLowering<'_> {
    /// A completion-style result branch, stored or flattened by the export wrapper.
    pub(super) fn completion_store(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        case: usize,
        payload: Option<(TypeHandle, Value)>,
        address: Value,
    ) -> Result<()> {
        let tag = self.i32_const(ctx, case as i32);
        self.store(ctx, MemKind::I32U8, address, 0, tag);
        if let Some(abi) = ty.cases()[case] {
            let (mir, value) =
                payload.ok_or_else(|| anyhow!("WIT result payload requires a return value"))?;
            self.lower_store(ctx, abi, mir, value, address, ty.payload_offset())?;
        }
        Ok(())
    }

    pub(super) fn completion_flat(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        case: usize,
        payload: Option<(TypeHandle, Value)>,
    ) -> Result<Vec<Value>> {
        let mut out = vec![self.i32_const(ctx, case as i32)];
        let layout = ty.sum_flat();
        if let Some(abi) = ty.cases()[case] {
            let (mir, value) =
                payload.ok_or_else(|| anyhow!("WIT result payload requires a return value"))?;
            let mut flat = Vec::new();
            self.lower_flat_values(ctx, abi, mir, value, &mut flat)?;
            out.extend(
                flat.into_iter()
                    .zip(abi.flat())
                    .enumerate()
                    .map(|(i, (value, from))| self.coerce_flat(ctx, value, from, layout[i + 1])),
            );
        }
        for slot in &layout[out.len()..] {
            let zero = self.i64_const(ctx, 0);
            out.push(self.coerce_flat(ctx, zero, Flat::I64, *slot));
        }
        Ok(out)
    }

    /// Thrown scalars are checked against the declared WIT error payload before lowering.
    pub(super) fn completion_error(
        &mut self,
        ctx: &mut Context,
        completion: Completion<'_>,
        exception: Value,
    ) -> Result<Option<(TypeHandle, Value)>> {
        use perry_runtime::exception as exception_layout;
        let Some(err) = completion.err else {
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
            let matches = self.num(ctx, NumOpcode::I32And, vec![at_least, at_most]);
            let valid = self.new_block(ctx, vec![]);
            let invalid = self.new_block(ctx, vec![]);
            self.cond_br(ctx, matches, (valid, vec![]), (invalid, vec![]));
            self.block = invalid;
            self.task_free(ctx);
            let trap = UnreachableOp::new(ctx).get_operation();
            self.insert(ctx, trap);
            self.block = valid;
            let string = perry_mir::types::StringType::get(ctx).into();
            let mir = RecordType::get(
                ctx,
                identifier("perry_completion_error"),
                Some(vec![("message".into(), string), ("name".into(), string)]),
            )
            .map_err(|_| anyhow!("inconsistent completion Error record shape"))?
            .into();
            let name = self.runtime(ctx, Runtime::StdExceptionName, vec![exception]);
            let message = self.runtime(ctx, Runtime::ExceptionMessage, vec![exception]);
            let value = self.build_record(
                ctx,
                mir,
                vec![("name".into(), name), ("message".into(), message)],
            )?;
            return Ok(Some((mir, value)));
        }
        let (expected, mir, kind) = match err {
            AbiType::String | AbiType::Enum(_) => (
                exception_layout::TAG_STRING,
                perry_mir::types::StringType::get(ctx).into(),
                MemKind::I32,
            ),
            AbiType::S64 | AbiType::U64 => (
                exception_layout::TAG_BIGINT,
                perry_mir::types::BigIntType::get(ctx).into(),
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
        let valid = self.new_block(ctx, vec![]);
        let invalid = self.new_block(ctx, vec![]);
        self.cond_br(ctx, matches, (valid, vec![]), (invalid, vec![]));
        self.block = invalid;
        self.task_free(ctx);
        let trap = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, trap);
        self.block = valid;
        let value = self.load(ctx, kind, exception, exception_layout::VALUE);
        if self.state.collectible && crate::repr(ctx, mir)? == crate::Repr::Ref {
            self.runtime(ctx, Runtime::GcPin, vec![value]);
        }
        Ok(Some((mir, value)))
    }
    pub(super) fn sum_switch(
        &mut self,
        ctx: &mut Context,
        tag: Value,
        count: usize,
        results: Vec<TypeHandle>,
        mut case: impl FnMut(&mut Self, &mut Context, usize) -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let join = self.new_block(ctx, results);
        for index in 0..count {
            let selected = self.new_block(ctx, vec![]);
            let next = self.new_block(ctx, vec![]);
            let discriminant = self.i32_const(ctx, index as i32);
            let matches = self.num(ctx, NumOpcode::I32Eq, vec![tag, discriminant]);
            self.cond_br(ctx, matches, (selected, vec![]), (next, vec![]));
            self.block = selected;
            let values = case(self, ctx, index)?;
            self.br(ctx, join, values);
            self.block = next;
        }
        let trap = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, trap);
        self.block = join;
        Ok(join.deref(ctx).arguments().collect())
    }

    fn payload_type(
        &self,
        ctx: &Context,
        ty: &AbiType,
        mir: TypeHandle,
        case: usize,
    ) -> Result<TypeHandle> {
        let field = match ty {
            AbiType::Option(_) => {
                return optional_value(ctx, mir)
                    .ok_or_else(|| anyhow!("WIT option needs a nullable TypeScript value"));
            }
            AbiType::Result { .. } => {
                let record = self.record_type(ctx, mir)?;
                record_field(ctx, record, if case == 0 { "value" } else { "error" })?.1
            }
            AbiType::Variant(cases) => {
                let variant = TypedHandle::<perry_mir::types::VariantType>::from_handle(mir, ctx)?;
                return variant
                    .deref(ctx)
                    .cases()
                    .iter()
                    .find(|(name, _)| *name == cases[case].0)
                    .and_then(|(_, payload)| *payload)
                    .ok_or_else(|| anyhow!("variant payload is missing"));
            }
            _ => bail!("sum has no payload"),
        };
        Ok(if matches!(ty.cases()[case], Some(AbiType::Option(_))) {
            field
        } else {
            optional_value(ctx, field).unwrap_or(field)
        })
    }

    fn lift_case(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        case: usize,
        payload: Option<Value>,
    ) -> Result<Value> {
        match ty {
            AbiType::Variant(cases) => {
                let payload = payload
                    .map(|value| -> Result<_> {
                        Ok((self.payload_type(ctx, ty, mir, case)?, value))
                    })
                    .transpose()?;
                self.variant_new(ctx, &cases[case].0, payload)
            }
            AbiType::Enum(names) => {
                ensure!(
                    mir.deref(ctx).is::<perry_mir::types::StringType>(),
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
                    Some(value) => Ok(value),
                    None if absence(ctx, mir).is_some_and(|a| a.admits_undefined()) => {
                        self.undefined(ctx, mir)
                    }
                    None => self.null(ctx, mir),
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
                for (name, field) in fields {
                    let value = if name.as_str() == "ok" {
                        ensure!(
                            crate::repr(ctx, field)? == crate::Repr::Bool,
                            "result ok field must be boolean"
                        );
                        self.i32_const(ctx, i32::from(case == 0))
                    } else if name.as_str() == if case == 0 { "value" } else { "error" } {
                        payload.ok_or_else(|| {
                            anyhow!("payload-free result has an unexpected {name} field")
                        })?
                    } else {
                        ensure!(
                            absence(ctx, field).is_some_and(|a| a.admits_undefined()),
                            "inactive result field {name} must admit undefined"
                        );
                        self.undefined(ctx, field)?
                    };
                    values.push((name.to_string(), value));
                }
                self.build_record(ctx, mir, values)
            }
            _ => unreachable!(),
        }
    }

    /// Reinterpret canonical joined slots; widening preserves the low payload bits.
    fn coerce_flat(&mut self, ctx: &mut Context, value: Value, from: Flat, to: Flat) -> Value {
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

    pub(super) fn lift_sum_flat(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        flat: &mut Vec<Value>,
    ) -> Result<Value> {
        let layout = ty.sum_flat();
        let raw = (0..layout.len())
            .map(|_| flat.pop().ok_or_else(|| anyhow!("too few flat values")))
            .collect::<Result<Vec<_>>>()?;
        let result = crate::wasm_type(ctx, mir)?;
        Ok(self.sum_switch(
            ctx,
            raw[0],
            ty.cases().len(),
            vec![result],
            |b, ctx, case| {
                let payload = if let Some(payload) = ty.cases()[case] {
                    let mir = b.payload_type(ctx, ty, mir, case)?;
                    let mut values = payload
                        .flat()
                        .into_iter()
                        .enumerate()
                        .map(|(i, target)| b.coerce_flat(ctx, raw[i + 1], layout[i + 1], target))
                        .collect::<Vec<_>>();
                    values.reverse();
                    Some(b.lift_flat(ctx, payload, mir, &mut values)?)
                } else {
                    None
                };
                Ok(vec![b.lift_case(ctx, ty, mir, case, payload)?])
            },
        )?[0])
    }

    pub(super) fn lift_sum_load(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        address: Value,
        offset: u32,
    ) -> Result<Value> {
        let tag = self.load(ctx, ty.discriminant_kind(), address, offset);
        let result = crate::wasm_type(ctx, mir)?;
        Ok(
            self.sum_switch(ctx, tag, ty.cases().len(), vec![result], |b, ctx, case| {
                let payload = if let Some(payload) = ty.cases()[case] {
                    let mir = b.payload_type(ctx, ty, mir, case)?;
                    Some(b.lift_load(ctx, payload, mir, address, offset + ty.payload_offset())?)
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
        value: Value,
    ) -> Result<Value> {
        Ok(match ty {
            AbiType::Option(_) => {
                ensure!(
                    optional_value(ctx, mir).is_some(),
                    "WIT option needs a nullable TypeScript value"
                );
                let absent = self.is_absent(ctx, value, mir)?;
                self.num(ctx, NumOpcode::I32Eqz, vec![absent])
            }
            AbiType::Result { .. } => {
                let record = self.record_type(ctx, mir)?;
                let (index, field) = record_field(ctx, record, "ok")?;
                ensure!(
                    crate::repr(ctx, field)? == crate::Repr::Bool,
                    "result ok field must be boolean"
                );
                let ok = self.load(ctx, MemKind::I32, value, 8 + 8 * index as u32);
                self.num(ctx, NumOpcode::I32Eqz, vec![ok])
            }
            AbiType::Variant(cases) => {
                ensure!(
                    mir.deref(ctx).is::<perry_mir::types::VariantType>(),
                    "variant needs a closed tagged union"
                );
                let tag = self.load(ctx, MemKind::I32, value, 0);
                let names = AbiType::Enum(cases.iter().map(|(name, _)| name.clone()).collect());
                let field = perry_mir::types::StringType::get(ctx).into();
                self.sum_tag(ctx, &names, field, tag)?
            }
            AbiType::Enum(names) => {
                ensure!(
                    mir.deref(ctx).is::<perry_mir::types::StringType>(),
                    "WIT enum needs a string literal union"
                );
                let mut tag = self.i32_const(ctx, -1);
                for (index, name) in names.iter().enumerate() {
                    let text = self.state.data.string(name);
                    let text = self.i32_const(ctx, text as i32);
                    let comparison = self.runtime(ctx, Runtime::StringCompare, vec![value, text]);
                    let matches = self.num(ctx, NumOpcode::I32Eqz, vec![comparison]);
                    let index = self.i32_const(ctx, index as i32);
                    tag = self.select(ctx, matches, index, tag);
                }
                tag
            }
            _ => unreachable!(),
        })
    }

    fn sum_payload(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        value: Value,
        case: usize,
    ) -> Result<Value> {
        if matches!(ty, AbiType::Option(_)) {
            return Ok(value);
        }
        if matches!(ty, AbiType::Variant(_)) {
            let payload = self.payload_type(ctx, ty, mir, case)?;
            return Ok(self.load(ctx, crate::slot_kind(ctx, payload)?, value, 8));
        }
        let record = self.record_type(ctx, mir)?;
        let name = if case == 0 { "value" } else { "error" };
        let (index, field) = record_field(ctx, record, name)?;
        let kind = crate::slot_kind(ctx, field)?;
        let value = self.load(ctx, kind, value, 8 + 8 * index as u32);
        if !matches!(ty.cases()[case], Some(AbiType::Option(_)))
            && optional_value(ctx, field).is_some()
        {
            let absent = self.is_absent(ctx, value, field)?;
            let trap = self.new_block(ctx, vec![]);
            let present = self.new_block(ctx, vec![]);
            self.cond_br(ctx, absent, (trap, vec![]), (present, vec![]));
            self.block = trap;
            let unreachable = UnreachableOp::new(ctx).get_operation();
            self.insert(ctx, unreachable);
            self.block = present;
        }
        Ok(value)
    }

    pub(super) fn lower_sum_store(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        value: Value,
        address: Value,
        offset: u32,
    ) -> Result<()> {
        let tag = self.sum_tag(ctx, ty, mir, value)?;
        self.store(ctx, ty.discriminant_kind(), address, offset, tag);
        self.sum_switch(ctx, tag, ty.cases().len(), vec![], |b, ctx, case| {
            if let Some(payload) = ty.cases()[case] {
                let payload_mir = b.payload_type(ctx, ty, mir, case)?;
                let payload_value = b.sum_payload(ctx, ty, mir, value, case)?;
                b.lower_store(
                    ctx,
                    payload,
                    payload_mir,
                    payload_value,
                    address,
                    offset + ty.payload_offset(),
                )?;
            }
            Ok(vec![])
        })?;
        Ok(())
    }

    pub(super) fn lower_sum_flat(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir: TypeHandle,
        value: Value,
    ) -> Result<Vec<Value>> {
        let tag = self.sum_tag(ctx, ty, mir, value)?;
        let layout = ty.sum_flat();
        let results = layout.iter().map(|ty| flat_type(ctx, *ty)).collect();
        self.sum_switch(ctx, tag, ty.cases().len(), results, |b, ctx, case| {
            let mut out = vec![tag];
            if let Some(payload) = ty.cases()[case] {
                let payload_mir = b.payload_type(ctx, ty, mir, case)?;
                let payload_value = b.sum_payload(ctx, ty, mir, value, case)?;
                let mut values = Vec::new();
                b.lower_flat_values(ctx, payload, payload_mir, payload_value, &mut values)?;
                out.extend(
                    values
                        .into_iter()
                        .zip(payload.flat())
                        .enumerate()
                        .map(|(i, (value, from))| b.coerce_flat(ctx, value, from, layout[i + 1])),
                );
            }
            for slot in &layout[out.len()..] {
                let zero = b.i64_const(ctx, 0);
                out.push(b.coerce_flat(ctx, zero, Flat::I64, *slot));
            }
            Ok(out)
        })
    }
}
