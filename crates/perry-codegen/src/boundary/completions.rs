//! Canonical result cases for returned values and uncaught exceptions.
use super::*;
use anyhow::Context as _;
use perry_mir::types::PromiseType;
use perry_runtime::{exception, promise};

#[derive(Clone, Copy)]
pub(super) enum EntryResult {
    Direct {
        result: Option<TypeHandle>,
        throws: bool,
    },
    Async(TypeHandle),
    Forwarded {
        result: TypeHandle,
        throws: bool,
    },
}

impl EntryResult {
    pub(super) fn new(
        ctx: &Context,
        state: &ModuleState,
        function: &FuncOp,
        results: &[TypeHandle],
    ) -> Result<Self> {
        ensure!(
            results.len() <= 1,
            "component implementations return at most one value"
        );
        let symbol = function.get_symbol_name(ctx);
        let result = results.first().copied();
        if state.async_functions.contains(&symbol) {
            return Ok(Self::Async(
                result.context("async implementation has no settled type")?,
            ));
        }
        let throws = state.throws.contains(&symbol);
        if let Some(result) = result.and_then(|ty| {
            ty.deref(ctx)
                .downcast_ref::<PromiseType>()
                .map(PromiseType::value)
        }) {
            Ok(Self::Forwarded { result, throws })
        } else {
            Ok(Self::Direct { result, throws })
        }
    }

    pub(super) fn settled_type(self) -> Option<TypeHandle> {
        match self {
            Self::Direct { result, .. } => result,
            Self::Async(result) | Self::Forwarded { result, .. } => Some(result),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Completion {
    Call {
        result: Option<(TypeHandle, Value)>,
        status: Value,
    },
    Promise {
        result: Option<TypeHandle>,
        promise: Value,
    },
}

struct Payload<'a> {
    ty: &'a AbiType,
    offset: u32,
    mir_ty: TypeHandle,
    value: Value,
}

impl FunctionLowering<'_> {
    pub(super) fn invoke_export(
        &mut self,
        ctx: &mut Context,
        function: &FuncOp,
        args: Vec<Value>,
        entry: EntryResult,
    ) -> Result<Completion> {
        if let EntryResult::Async(result) = entry {
            let i32 = i32_type(ctx);
            let call = CallOp::new(ctx, function.get_symbol_name(ctx), args, vec![i32]);
            let promise = self.result(ctx, call.get_operation());
            return Ok(Completion::Promise {
                result: Some(result),
                promise,
            });
        }
        let (mut types, throws) = match entry {
            EntryResult::Direct { result, throws } => (
                result
                    .into_iter()
                    .map(|ty| crate::wasm_type(ctx, ty))
                    .collect::<Result<Vec<_>>>()?,
                throws,
            ),
            EntryResult::Forwarded { throws, .. } => (vec![i32_type(ctx)], throws),
            EntryResult::Async(_) => unreachable!(),
        };
        if throws {
            types.push(i32_type(ctx));
        }
        let call = CallOp::new(ctx, function.get_symbol_name(ctx), args, types).get_operation();
        self.insert(ctx, call);
        let values: Vec<_> = call.deref(ctx).results().collect();
        let status = if throws {
            *values.last().unwrap()
        } else {
            self.i32_const(ctx, 0)
        };
        match entry {
            EntryResult::Direct { result, .. } => Ok(Completion::Call {
                result: result.map(|ty| (ty, values[0])),
                status,
            }),
            EntryResult::Forwarded { result, .. } => {
                let promise = if throws {
                    let i32 = i32_type(ctx);
                    let done = self.new_block(ctx, vec![i32]);
                    let failed = self.new_block(ctx, vec![]);
                    self.cond_br(ctx, status, (failed, vec![]), (done, vec![values[0]]));
                    self.block = failed;
                    let rejected = self
                        .runtime(ctx, Runtime::PromiseNew, vec![])
                        .expect("rejected promise");
                    self.runtime(ctx, Runtime::PromiseReject, vec![rejected, status]);
                    self.br(ctx, done, vec![rejected]);
                    self.block = done;
                    done.deref(ctx).get_argument(0)
                } else {
                    values[0]
                };
                Ok(Completion::Promise {
                    result: Some(result),
                    promise,
                })
            }
            EntryResult::Async(_) => unreachable!(),
        }
    }

    /// The async WIT driver retains a native synchronous completion in a root promise.
    pub(super) fn completion_promise(
        &mut self,
        ctx: &mut Context,
        completion: Completion,
    ) -> Value {
        let (result, status) = match completion {
            Completion::Call { result, status } => (result, status),
            Completion::Promise { promise, .. } => return promise,
        };
        let promise = self
            .runtime(ctx, Runtime::PromiseNew, vec![])
            .expect("root promise");
        let fulfilled = self.new_block(ctx, vec![]);
        let rejected = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.cond_br(ctx, status, (rejected, vec![]), (fulfilled, vec![]));
        self.block = fulfilled;
        let bits = match result {
            Some((_, value)) => self.slot_bits(ctx, value),
            None => self.i64_const(ctx, crate::UNDEFINED_BITS),
        };
        self.runtime(ctx, Runtime::PromiseResolve, vec![promise, bits]);
        self.br(ctx, done, vec![]);
        self.block = rejected;
        self.runtime(ctx, Runtime::PromiseReject, vec![promise, status]);
        self.br(ctx, done, vec![]);
        self.block = done;
        promise
    }

    pub(super) fn lower_completion(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        completion: Completion,
        indirect: bool,
    ) -> Result<Vec<Value>> {
        let AbiType::Result { ok, err } = ty else {
            bail!("completion adaptation requires a WIT result");
        };
        let flat = ty.flat();
        let payload_offset = ty.alignment();
        let area = if indirect {
            let size = self.i32_const(ctx, ty.size() as i32);
            Some(
                self.runtime(ctx, Runtime::Alloc, vec![size])
                    .expect("result area"),
            )
        } else {
            None
        };
        let types = if indirect {
            Vec::new()
        } else {
            flat.iter().map(|ty| flat_type(ctx, *ty)).collect()
        };
        let done = self.new_block(ctx, types);
        let returned = self.new_block(ctx, vec![]);
        let thrown = self.new_block(ctx, vec![]);
        let status = match completion {
            Completion::Call { status, .. } => status,
            Completion::Promise { promise, .. } => {
                self.load(ctx, MemKind::I32, promise, promise::STATUS)
            }
        };
        self.cond_br(ctx, status, (thrown, vec![]), (returned, vec![]));
        for (index, case, block) in [(0, ok, returned), (1, err, thrown)] {
            self.block = block;
            let payload = if index == 0 {
                if let Some(ty) = case {
                    let (mir_ty, value) = match completion {
                        Completion::Call { result, .. } => {
                            result.context("WIT result success needs a return value")?
                        }
                        Completion::Promise { result, promise } => {
                            let ty = result.context("WIT result success needs a promise value")?;
                            let kind = crate::slot_kind(ctx, ty)?;
                            (ty, self.load(ctx, kind, promise, promise::VALUE))
                        }
                    };
                    vec![Payload {
                        ty,
                        offset: 0,
                        mir_ty,
                        value,
                    }]
                } else {
                    Vec::new()
                }
            } else {
                let exception = match completion {
                    Completion::Call { status, .. } => self
                        .runtime(ctx, Runtime::ExceptionCatch, vec![status])
                        .expect("exception"),
                    Completion::Promise { promise, .. } => {
                        self.load(ctx, MemKind::I32, promise, promise::VALUE)
                    }
                };
                case.as_ref()
                    .map(|ty| self.result_exception(ctx, ty, exception))
                    .transpose()?
                    .unwrap_or_default()
            };
            if matches!(completion, Completion::Call { .. }) {
                self.finish_sync(ctx);
            }
            let tag = self.i32_const(ctx, index);
            if let Some(area) = area {
                self.store(ctx, MemKind::I32U8, area, 0, tag);
                for part in payload {
                    self.lower_store(
                        ctx,
                        part.ty,
                        part.mir_ty,
                        part.value,
                        area,
                        payload_offset + part.offset,
                    )?;
                }
                self.br(ctx, done, vec![]);
            } else {
                let mut values = Vec::new();
                for part in payload {
                    self.lower_flat_values(ctx, part.ty, part.mir_ty, part.value, &mut values)?;
                }
                let source_types = case.as_ref().map_or_else(Vec::new, |ty| ty.flat());
                let mut joined = vec![tag];
                for (i, target) in flat[1..].iter().copied().enumerate() {
                    let value = if let Some(value) = values.get(i) {
                        self.join_result_value(ctx, *value, source_types[i], target)
                    } else {
                        let ty = flat_type(ctx, target);
                        self.zero(ctx, ty)
                    };
                    joined.push(value);
                }
                self.br(ctx, done, joined);
            }
        }
        self.block = done;
        Ok(match area {
            Some(area) => vec![area],
            None => done.deref(ctx).arguments().collect(),
        })
    }

    fn result_exception<'a>(
        &mut self,
        ctx: &mut Context,
        ty: &'a AbiType,
        value: Value,
    ) -> Result<Vec<Payload<'a>>> {
        let (tag, mir_ty): (u32, TypeHandle) = match ty {
            AbiType::Bool => (
                exception::BOOLEAN,
                IntegerType::get(ctx, 1, Signedness::Signless).into(),
            ),
            AbiType::String => (exception::STRING, StringType::get(ctx).into()),
            AbiType::Record(fields) => {
                ensure!(
                    fields
                        .iter()
                        .all(|(name, ty)| matches!(name.as_str(), "name" | "message")
                            && *ty == AbiType::String),
                    "WIT Error records require string fields named name or message"
                );
                (exception::ERROR, StringType::get(ctx).into())
            }
            AbiType::U8
            | AbiType::S8
            | AbiType::U16
            | AbiType::S16
            | AbiType::U32
            | AbiType::S32
            | AbiType::U64
            | AbiType::S64
            | AbiType::F32
            | AbiType::F64 => (exception::NUMBER, FP64Type::get(ctx).into()),
            _ => bail!(
                "WIT completion errors require a primitive payload, an Error record or no payload"
            ),
        };
        let tag = self.i32_const(ctx, tag as i32);
        let matches = self
            .runtime(ctx, Runtime::ExceptionIs, vec![value, tag])
            .expect("exception kind");
        let valid = self.new_block(ctx, vec![]);
        let trap = self.new_block(ctx, vec![]);
        self.cond_br(ctx, matches, (valid, vec![]), (trap, vec![]));
        self.block = trap;
        self.abort_task(ctx);
        let unreachable = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, unreachable);
        self.block = valid;
        let bits = self.load(ctx, MemKind::I64, value, exception::VALUE);
        if let AbiType::Record(fields) = ty {
            let message = self.num(ctx, NumOpcode::I32WrapI64, vec![bits]);
            let mut name = None;
            if fields.iter().any(|(name, _)| name == "name") {
                let kind = self.load(ctx, MemKind::I32, value, exception::KIND);
                let address = self.state.data.string("Error");
                let mut selected = self.i32_const(ctx, address as i32);
                for (expected, text) in [
                    (exception::TYPE_ERROR, "TypeError"),
                    (exception::RANGE_ERROR, "RangeError"),
                    (exception::SYNTAX_ERROR, "SyntaxError"),
                ] {
                    let expected = self.i32_const(ctx, expected as i32);
                    let matches = self.num(ctx, NumOpcode::I32Eq, vec![kind, expected]);
                    let address = self.state.data.string(text);
                    let text = self.i32_const(ctx, address as i32);
                    selected = self.select(ctx, matches, text, selected);
                }
                name = Some(selected);
            }
            return Ok(fields
                .iter()
                .zip(ty.field_offsets())
                .map(|((field, ty), offset)| Payload {
                    ty,
                    offset,
                    mir_ty,
                    value: if field == "name" {
                        name.expect("name field")
                    } else {
                        message
                    },
                })
                .collect());
        }
        let opcode = if mir_ty.deref(ctx).is::<FP64Type>() {
            NumOpcode::F64ReinterpretI64
        } else {
            NumOpcode::I32WrapI64
        };
        Ok(vec![Payload {
            ty,
            offset: 0,
            mir_ty,
            value: self.num(ctx, opcode, vec![bits]),
        }])
    }

    fn join_result_value(
        &mut self,
        ctx: &mut Context,
        value: Value,
        source: Flat,
        target: Flat,
    ) -> Value {
        if source == target {
            return value;
        }
        let value = if source == Flat::F32 {
            self.num(ctx, NumOpcode::I32ReinterpretF32, vec![value])
        } else {
            value
        };
        match (source, target) {
            (Flat::F32, Flat::I32) => value,
            (Flat::I32 | Flat::F32, Flat::I64) => {
                self.num(ctx, NumOpcode::I64ExtendI32U, vec![value])
            }
            (Flat::F64, Flat::I64) => self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]),
            _ => unreachable!("canonical result join"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_layouts_join_types_without_numeric_conversion() {
        for (ok, err, flat, size, alignment) in [
            (None, None, vec![Flat::I32], 1, 1),
            (
                Some(AbiType::Bool),
                Some(AbiType::F64),
                vec![Flat::I32, Flat::I64],
                16,
                8,
            ),
            (
                Some(AbiType::F32),
                Some(AbiType::U32),
                vec![Flat::I32, Flat::I32],
                8,
                4,
            ),
            (
                Some(AbiType::F64),
                Some(AbiType::F64),
                vec![Flat::I32, Flat::F64],
                16,
                8,
            ),
            (
                Some(AbiType::TextOrBytes(TextBytesOrder::BytesFirst)),
                Some(AbiType::F64),
                vec![Flat::I32, Flat::I64, Flat::I32, Flat::I32],
                24,
                8,
            ),
        ] {
            let ty = AbiType::Result {
                ok: ok.map(Box::new),
                err: err.map(Box::new),
            };
            assert_eq!(ty.flat(), flat);
            assert_eq!(ty.size(), size);
            assert_eq!(ty.alignment(), alignment);
        }
    }
}
