//! Runtime and host intrinsic lowering.
use crate::strings;
use crate::{
    MONOTONIC_CLOCK,
    function::FunctionLowering,
    repr::{Carriers, Repr, repr},
    runtime::{Runtime, W},
};
use anyhow::{Result, bail, ensure};
use perry_mir::{Intrinsic, NumericType, absence};
use pliron::{
    context::{Context, Ptr},
    operation::Operation,
    r#type::{Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_, '_> {
    fn needs_import(&self, interface: &str, what: &str) -> Result<()> {
        ensure!(
            self.state.can_import(interface),
            "{what} needs the WIT world to import {interface}"
        );
        Ok(())
    }

    pub(super) fn intrinsic(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        intrinsic: Intrinsic,
    ) -> Result<()> {
        if intrinsic == Intrinsic::ParseInt {
            let str_op = op.deref(ctx).get_operand(0);
            let str_val = self.mapped(ctx, str_op)?;
            let (radix_tag, radix_payload) = self.numeric_operand(ctx, op, 1)?;
            let out = *self
                .state
                .numeric_result
                .get_or_insert_with(|| self.state.data.append(8, &[0; 8]));
            let out_ptr = self.i32_const(ctx, out as i32);
            let tag = self.runtime(
                ctx,
                Runtime::StdParseInt,
                vec![str_val, radix_tag, radix_payload, out_ptr],
            );
            let payload = self.load(ctx, MemKind::I64, out_ptr, 0);
            let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
            let numeric_ty = NumericType::get(ctx).into();
            let value =
                self.cast_value(ctx, Carriers::Numeric { tag, payload }, numeric_ty, res_ty)?;
            self.define_value(ctx, op, value);
            return Ok(());
        }
        // Integer and tagged arguments keep their exact value; floats take the generic paths.
        let exact_number = |ctx: &Context| {
            let arg_ty = op.deref(ctx).get_operand(0).get_type(ctx);
            arg_ty.deref(ctx).is::<NumericType>() || crate::numeric::integer(ctx, arg_ty).is_some()
        };
        if intrinsic == Intrinsic::NumberToString && exact_number(ctx) {
            let (tag, payload) = self.numeric_operand(ctx, op, 0)?;
            let norm_tag = self.canonical_numeric_tag(ctx, tag);
            let value = self.runtime(ctx, Runtime::StdNumberToString, vec![norm_tag, payload]);
            self.define(ctx, op, value);
            return Ok(());
        }
        let predicate = match intrinsic {
            Intrinsic::NumberIsNaN => Some(NumberPredicate::NaN),
            Intrinsic::NumberIsFinite => Some(NumberPredicate::Finite),
            Intrinsic::NumberIsInteger => Some(NumberPredicate::Integer),
            Intrinsic::NumberIsSafeInteger => Some(NumberPredicate::SafeInteger),
            _ => None,
        };
        if let Some(predicate) = predicate
            && exact_number(ctx)
        {
            let result = self.number_predicate(ctx, op, predicate)?;
            self.define(ctx, op, result);
            return Ok(());
        }
        if let Some(value) = self.numeric_intrinsic(ctx, op, intrinsic)? {
            self.define_value(ctx, op, value);
            return Ok(());
        }
        let args = self.operands(ctx, op)?;
        let value = match intrinsic {
            Intrinsic::ExceptionEqual => Some(self.runtime(ctx, Runtime::StdExceptionEqual, args)),
            Intrinsic::ExceptionBigIntEqual => {
                Some(self.runtime(ctx, Runtime::StdExceptionBigIntEqual, args))
            }
            Intrinsic::ModuleInitialize => Some(self.initialization_promise(ctx)?),
            Intrinsic::BytesByteInput | Intrinsic::UrlSearchParams => Some(args[0]),
            Intrinsic::PrintOut | Intrinsic::PrintErr => {
                let (interface, fd) = if intrinsic == Intrinsic::PrintOut {
                    ("wasi:cli/stdout@0.3.0", 0)
                } else {
                    ("wasi:cli/stderr@0.3.0", 1)
                };
                self.needs_import(interface, "console output")?;
                self.state.uses_io = true;
                let fd = self.i32_const(ctx, fd);
                let newline = self.i32_const(ctx, 1);
                self.runtime_void(ctx, Runtime::Print, vec![fd, args[0], newline]);
                None
            }
            Intrinsic::NumberToString => {
                let size = self.i32_const(ctx, 40);
                let data = self.alloc(ctx, size);
                let len = self.runtime(ctx, Runtime::NumberFormat, vec![args[0], data]);
                Some(self.runtime(ctx, Runtime::StringAdopt, vec![data, len]))
            }
            Intrinsic::EnvGet | Intrinsic::EnvKeys => {
                self.needs_import("wasi:cli/environment@0.3.0", "process.env")?;
                let function = if intrinsic == Intrinsic::EnvGet {
                    Runtime::EnvGet
                } else {
                    Runtime::EnvKeys
                };
                Some(self.runtime(ctx, function, args))
            }
            Intrinsic::PerformanceNow => {
                self.needs_import(MONOTONIC_CLOCK, "performance.now()")?;
                Some(self.runtime(ctx, Runtime::PerformanceNow, args))
            }
            Intrinsic::DateNow => {
                self.needs_import("wasi:clocks/system-clock@0.3.0", "Date.now()")?;
                Some(self.runtime(ctx, Runtime::DateNow, args))
            }
            Intrinsic::ExitCodeSet => {
                self.runtime_void(ctx, Runtime::ExitCodeSet, args);
                None
            }
            Intrinsic::MathFloor
            | Intrinsic::MathCeil
            | Intrinsic::MathTrunc
            | Intrinsic::MathRound
                if crate::numeric::integer(ctx, op.deref(ctx).get_result(0).get_type(ctx))
                    .is_some() =>
            {
                Some(args[0])
            }
            Intrinsic::MathFloor => Some(self.num(ctx, NumOpcode::F64Floor, args)),
            Intrinsic::MathCeil => Some(self.num(ctx, NumOpcode::F64Ceil, args)),
            Intrinsic::MathTrunc => Some(self.num(ctx, NumOpcode::F64Trunc, args)),
            Intrinsic::MathAbs => {
                let ty = op.deref(ctx).get_result(0).get_type(ctx);
                if let Some((bits, signed)) = crate::numeric::integer(ctx, ty) {
                    if !signed {
                        Some(args[0])
                    } else if bits == 64 {
                        let x = args[0];
                        let zero = self.i64_const(ctx, 0);
                        let neg = self.num(ctx, NumOpcode::I64Sub, vec![zero, x]);
                        let is_neg = self.num(ctx, NumOpcode::I64LtS, vec![x, zero]);
                        Some(self.select(ctx, is_neg, neg, x))
                    } else {
                        let x = args[0];
                        let zero = self.i32_const(ctx, 0);
                        let neg = self.num(ctx, NumOpcode::I32Sub, vec![zero, x]);
                        let is_neg = self.num(ctx, NumOpcode::I32LtS, vec![x, zero]);
                        let abs = self.select(ctx, is_neg, neg, x);
                        Some(self.normalize_integer(ctx, abs, ty))
                    }
                } else {
                    Some(self.num(ctx, NumOpcode::F64Abs, args))
                }
            }
            Intrinsic::MathSqrt => Some(self.num(ctx, NumOpcode::F64Sqrt, args)),
            Intrinsic::BytesAt => {
                let data = self.load(ctx, MemKind::I32, args[0], perry_abi::BYTES_DATA as u32);
                let address = self.num(ctx, NumOpcode::I32Add, vec![data, args[1]]);
                let byte = self.load(ctx, MemKind::I32U8, address, 0);
                let ty = op.deref(ctx).get_result(0).get_type(ctx);
                if let Some((bits, _)) = crate::numeric::integer(ctx, ty) {
                    if bits == 64 {
                        Some(self.num(ctx, NumOpcode::I64ExtendI32U, vec![byte]))
                    } else {
                        Some(self.normalize_integer(ctx, byte, ty))
                    }
                } else {
                    Some(self.num(ctx, NumOpcode::F64ConvertI32U, vec![byte]))
                }
            }
            Intrinsic::MathMin | Intrinsic::MathMax => {
                let ty = op.deref(ctx).get_result(0).get_type(ctx);
                if let Some((bits, signed)) = crate::numeric::integer(ctx, ty) {
                    let cmp_op = match (bits == 64, signed) {
                        (false, false) => NumOpcode::I32LtU,
                        (false, true) => NumOpcode::I32LtS,
                        (true, false) => NumOpcode::I64LtU,
                        (true, true) => NumOpcode::I64LtS,
                    };
                    let lt = self.num(ctx, cmp_op, vec![args[0], args[1]]);
                    if intrinsic == Intrinsic::MathMin {
                        Some(self.select(ctx, lt, args[0], args[1]))
                    } else {
                        Some(self.select(ctx, lt, args[1], args[0]))
                    }
                } else if intrinsic == Intrinsic::MathMin {
                    Some(self.num(ctx, NumOpcode::F64Min, args))
                } else {
                    Some(self.num(ctx, NumOpcode::F64Max, args))
                }
            }
            Intrinsic::MathRound => {
                // floor(x), plus one when the fraction is at least ½; the result keeps the
                // sign of x, so -0.4 rounds to -0 as in ECMAScript.
                let x = args[0];
                let floor = self.num(ctx, NumOpcode::F64Floor, vec![x]);
                let fraction = self.num(ctx, NumOpcode::F64Sub, vec![x, floor]);
                let half = self.f64_const(ctx, 0.5);
                let up = self.num(ctx, NumOpcode::F64Ge, vec![fraction, half]);
                let one = self.f64_const(ctx, 1.0);
                let next = self.num(ctx, NumOpcode::F64Add, vec![floor, one]);
                let rounded = self.select(ctx, up, next, floor);
                Some(self.num(ctx, NumOpcode::F64Copysign, vec![rounded, x]))
            }
            Intrinsic::ParseInt => Some(self.runtime(ctx, Runtime::StdParseInt, args)),
            Intrinsic::ParseFloat => Some(self.runtime(ctx, Runtime::StdParseFloat, args)),
            Intrinsic::StringToNumber => Some(self.runtime(ctx, Runtime::StdStringToNumber, args)),
            Intrinsic::NumberIsNaN => {
                let x = args[0];
                Some(self.num(ctx, NumOpcode::F64Ne, vec![x, x]))
            }
            Intrinsic::NumberIsFinite => {
                let abs = self.num(ctx, NumOpcode::F64Abs, vec![args[0]]);
                let inf = self.f64_const(ctx, f64::INFINITY);
                Some(self.num(ctx, NumOpcode::F64Lt, vec![abs, inf]))
            }
            Intrinsic::NumberIsInteger => {
                let x = args[0];
                let abs = self.num(ctx, NumOpcode::F64Abs, vec![x]);
                let inf = self.f64_const(ctx, f64::INFINITY);
                let finite = self.num(ctx, NumOpcode::F64Lt, vec![abs, inf]);
                let trunc = self.num(ctx, NumOpcode::F64Trunc, vec![x]);
                let integral = self.num(ctx, NumOpcode::F64Eq, vec![trunc, x]);
                Some(self.num(ctx, NumOpcode::I32And, vec![finite, integral]))
            }
            Intrinsic::NumberIsSafeInteger => {
                let x = args[0];
                let abs = self.num(ctx, NumOpcode::F64Abs, vec![x]);
                let max_safe = self.f64_const(ctx, 9_007_199_254_740_991.0);
                let in_range = self.num(ctx, NumOpcode::F64Le, vec![abs, max_safe]);
                let trunc = self.num(ctx, NumOpcode::F64Trunc, vec![x]);
                let integral = self.num(ctx, NumOpcode::F64Eq, vec![trunc, x]);
                Some(self.num(ctx, NumOpcode::I32And, vec![in_range, integral]))
            }
            Intrinsic::DateFromTime
            | Intrinsic::DateTime
            | Intrinsic::DateParse
            | Intrinsic::DateUtc
            | Intrinsic::DatePart
            | Intrinsic::DateSet
            | Intrinsic::DateIso
            | Intrinsic::DateJson => Some(self.date_intrinsic(ctx, intrinsic, args)?),
            Intrinsic::DecoderFatal | Intrinsic::DecoderIgnoreBom => {
                let offset = if intrinsic == Intrinsic::DecoderFatal {
                    0
                } else {
                    4
                };
                Some(self.load(ctx, MemKind::I32, args[0], offset))
            }
            Intrinsic::StringLower | Intrinsic::StringUpper => {
                use perry_abi::layout::string::{DATA, LEN};
                let string = args[0];
                let data = self.load(ctx, MemKind::I32, string, DATA);
                let len = self.load(ctx, MemKind::I32, string, LEN);
                let upper = self.i32_const(ctx, i32::from(intrinsic == Intrinsic::StringUpper));
                let zero = self.i32_const(ctx, 0);
                let measured =
                    self.runtime(ctx, Runtime::StrCaseConvert, vec![data, len, zero, upper]);
                let bytes = self.num(ctx, NumOpcode::I32WrapI64, vec![measured]);
                let changed = self.new_block(ctx, vec![]);
                let i32 = crate::repr::i32_type(ctx);
                let done = self.new_block(ctx, vec![i32]);
                self.cond_br(ctx, bytes, (changed, vec![]), (done, vec![string]));
                self.block = changed;
                let out = self.alloc(ctx, bytes);
                self.runtime(ctx, Runtime::StrCaseConvert, vec![data, len, out, upper]);
                let shift = self.i64_const(ctx, 32);
                let scalars = self.num(ctx, NumOpcode::I64ShrU, vec![measured, shift]);
                let scalars = self.num(ctx, NumOpcode::I32WrapI64, vec![scalars]);
                let text =
                    self.runtime(ctx, Runtime::StringAdoptCounted, vec![out, bytes, scalars]);
                self.br(ctx, done, vec![text]);
                self.block = done;
                Some(done.deref(ctx).get_argument(0))
            }
            Intrinsic::StringCharAtIndex | Intrinsic::StringScalarAtIndex => {
                let representation = if intrinsic == Intrinsic::StringCharAtIndex {
                    strings::Character::String
                } else {
                    strings::Character::Scalar
                };
                Some(self.char_at_index(ctx, args[0], args[1], representation))
            }
            Intrinsic::StringCharAt | Intrinsic::StringScalarAt => {
                let representation = if intrinsic == Intrinsic::StringCharAt {
                    strings::Character::String
                } else {
                    strings::Character::Scalar
                };
                Some(self.char_at(ctx, args[0], args[1], representation))
            }
            Intrinsic::StringTryAppend | Intrinsic::StringScalarTryAppend => {
                let representation = if intrinsic == Intrinsic::StringTryAppend {
                    strings::Character::String
                } else {
                    strings::Character::Scalar
                };
                Some(self.try_string_append(ctx, args[0], args[1], representation))
            }
            Intrinsic::StringScalarLength => {
                let zero = self.i32_const(ctx, 0);
                Some(self.num(ctx, NumOpcode::I32GeS, vec![args[0], zero]))
            }
            Intrinsic::JsonCursorBegin => Some(self.runtime(ctx, Runtime::JsonCursorBegin, args)),
            Intrinsic::JsonCursorByte => Some(self.runtime(ctx, Runtime::JsonCursorByte, args)),
            Intrinsic::JsonCursorNumber => Some(self.runtime(ctx, Runtime::JsonCursorNumber, args)),
            Intrinsic::JsonCursorI64 => Some(self.runtime(ctx, Runtime::JsonCursorI64, args)),
            Intrinsic::JsonCursorU64 => Some(self.runtime(ctx, Runtime::JsonCursorU64, args)),
            Intrinsic::JsonToI64 => Some(self.runtime(ctx, Runtime::StdJsonToI64, args)),
            Intrinsic::JsonToU64 => Some(self.runtime(ctx, Runtime::StdJsonToU64, args)),
            Intrinsic::JsonCursorBoolean => {
                Some(self.runtime(ctx, Runtime::JsonCursorBoolean, args))
            }
            Intrinsic::JsonCursorString => Some(self.runtime(ctx, Runtime::JsonCursorString, args)),
            Intrinsic::JsonCursorValue => Some(self.runtime(ctx, Runtime::JsonCursorValue, args)),
            Intrinsic::JsonCursorMember => Some(self.runtime(ctx, Runtime::JsonCursorMember, args)),
            Intrinsic::JsonCursorConsume => {
                self.runtime_void(ctx, Runtime::JsonCursorConsume, args);
                None
            }
            Intrinsic::JsonCursorDiscard => {
                self.runtime_void(ctx, Runtime::JsonCursorDiscard, args);
                None
            }
            Intrinsic::JsonCursorTail => Some(self.runtime(ctx, Runtime::JsonCursorTail, args)),
            Intrinsic::JsonCursorSeek => Some(self.runtime(ctx, Runtime::JsonCursorSeek, args)),
            Intrinsic::JsonCursorFinish => {
                self.runtime_void(ctx, Runtime::JsonCursorFinish, args);
                None
            }
            Intrinsic::JsonResidue => {
                let ty = op.deref(ctx).get_operand(0).get_type(ctx);
                let offset = if ty.deref(ctx).is::<perry_mir::RecordType>() {
                    perry_abi::layout::record::JSON
                } else {
                    perry_abi::layout::variant::JSON
                };
                self.store(ctx, MemKind::I32, args[0], offset, args[1]);
                None
            }
            Intrinsic::JsonRetain => {
                use perry_mir::{JsonType, RecordType};
                let ty = op.deref(ctx).get_operand(0).get_type(ctx);
                let (offset, graph) =
                    if let Ok(record) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
                        let required = record
                            .deref(ctx)
                            .fields()?
                            .iter()
                            .filter(|(_, ty)| {
                                !ty.deref(ctx).is::<JsonType>()
                                    && !absence(ctx, *ty)
                                        .is_some_and(|absence| absence.admits_undefined())
                            })
                            .count();
                        // Successful validation found every required member. An equal raw count
                        // leaves no room for extra members or duplicates, so no graph is needed.
                        let count = self.load(
                            ctx,
                            MemKind::I32,
                            args[1],
                            perry_abi::json::MEMBER_COUNT as u32,
                        );
                        let required = self.i32_const(ctx, required as i32);
                        let complete = self.num(ctx, NumOpcode::I32Eq, vec![count, required]);
                        let zero = self.i32_const(ctx, 0);
                        let graph = self.select(ctx, complete, zero, args[1]);
                        (perry_abi::layout::record::JSON, graph)
                    } else {
                        (perry_abi::layout::variant::JSON, args[1])
                    };
                self.store(ctx, MemKind::I32, args[0], offset, graph);
                None
            }
            Intrinsic::JsonParse => {
                let value = self.runtime(ctx, Runtime::JsonParseString, args);
                let status = self.runtime(ctx, Runtime::Status, vec![]);
                self.throw_status_if(ctx, status, status);
                Some(value)
            }
            Intrinsic::InstantFromMs
            | Intrinsic::InstantFrom
            | Intrinsic::PlainDateFrom
            | Intrinsic::PlainDateTimeFrom
            | Intrinsic::PlainDateAddDays
            | Intrinsic::PlainDateTimeAddDays
            | Intrinsic::InstantMs
            | Intrinsic::PlainDatePart
            | Intrinsic::PlainDateTimePart
            | Intrinsic::InstantString
            | Intrinsic::PlainDateString
            | Intrinsic::PlainDateTimeString => self.temporal_intrinsic(ctx, intrinsic, args),
            Intrinsic::AbortNew => Some(self.runtime(ctx, Runtime::AbortNew, args)),
            Intrinsic::AbortAbort => {
                let message = self.state.data.string("The operation was aborted");
                let message = self.i32_const(ctx, message as i32);
                let scope_error = self
                    .state
                    .data
                    .string("AbortSignal cancellation is limited to the current export task");
                let scope_error = self.i32_const(ctx, scope_error as i32);
                self.runtime_void(ctx, Runtime::Abort, vec![args[0], message, scope_error]);
                None
            }
            Intrinsic::AbortAborted => Some(self.runtime(ctx, Runtime::AbortAborted, args)),
            Intrinsic::JsonIsUndefined => Some(self.num(ctx, NumOpcode::I32Eqz, args)),
            Intrinsic::ExceptionSetCause => {
                let (exception, cause) = (args[0], args[1]);
                self.store(
                    ctx,
                    MemKind::I32,
                    exception,
                    perry_abi::layout::exception::CAUSE,
                    cause,
                );
                None
            }
            Intrinsic::JsonStrictEqual => {
                Some(self.runtime(ctx, Runtime::StdJsonStrictEqual, args))
            }
            Intrinsic::JsonExceptionEqual => {
                Some(self.runtime(ctx, Runtime::StdJsonExceptionEqual, args))
            }
            Intrinsic::JsonOriginEqual => {
                Some(self.runtime(ctx, Runtime::StdJsonOriginEqual, args))
            }
            Intrinsic::JsonFromException => {
                Some(self.runtime(ctx, Runtime::StdJsonFromException, args))
            }
            Intrinsic::JsonToException => {
                Some(self.runtime(ctx, Runtime::StdJsonToException, args))
            }
            Intrinsic::JsonExceptionIs => {
                Some(self.runtime(ctx, Runtime::StdJsonExceptionIs, args))
            }
            Intrinsic::ExceptionIsNull | Intrinsic::ExceptionIsUndefined => {
                let kind = self.load(
                    ctx,
                    MemKind::I32,
                    args[0],
                    perry_abi::layout::exception::KIND,
                );
                let tag = self.load(
                    ctx,
                    MemKind::I32,
                    args[0],
                    perry_abi::layout::exception::TAG,
                );
                let thrown = self.i32_const(ctx, perry_abi::layout::exception::THROWN_VALUE as i32);
                let is_value = self.num(ctx, NumOpcode::I32Eq, vec![kind, thrown]);
                let expected = self.i32_const(
                    ctx,
                    if intrinsic == Intrinsic::ExceptionIsNull {
                        perry_abi::layout::exception::TAG_NULL as i32
                    } else {
                        perry_abi::layout::exception::TAG_UNDEFINED as i32
                    },
                );
                let matches = self.num(ctx, NumOpcode::I32Eq, vec![tag, expected]);
                Some(self.num(ctx, NumOpcode::I32And, vec![is_value, matches]))
            }
            Intrinsic::ExceptionCause => Some(self.load(
                ctx,
                MemKind::I32,
                args[0],
                perry_abi::layout::exception::CAUSE,
            )),
            // The null address is the JSON `undefined`.
            Intrinsic::JsonUndefined => Some(self.i32_const(ctx, 0)),
            Intrinsic::JsonFromBool | Intrinsic::JsonNull => {
                // Node kinds of the JSON graph: 1 null, 2 boolean.
                let null = intrinsic == Intrinsic::JsonNull;
                let kind = self.i32_const(ctx, if null { 1 } else { 2 });
                let truth = match args.first() {
                    Some(&truth) => truth,
                    None => self.i32_const(ctx, 0),
                };
                Some(self.runtime(ctx, Runtime::StdJsonFromScalar, vec![kind, truth]))
            }
            Intrinsic::MathRandom => {
                self.needs_import("wasi:random/insecure@0.3.0", "Math.random()")?;
                Some(self.runtime(ctx, Runtime::StdMathRandom, args))
            }
            Intrinsic::RandomUuid => {
                self.needs_import("wasi:random/random@0.3.0", "crypto.randomUUID()")?;
                Some(self.runtime(ctx, Runtime::StdRandomUuid, args))
            }
            Intrinsic::BytesFillRandom => {
                self.needs_import("wasi:random/random@0.3.0", "crypto.getRandomValues()")?;
                Some(self.runtime(ctx, Runtime::StdBytesFillRandom, args))
            }
            Intrinsic::JsonSerialize => {
                bail!("JSON.stringify of parsed JSON values is not supported yet")
            }
            Intrinsic::WritableStdout | Intrinsic::WritableStderr => {
                let (interface, fd) = if intrinsic == Intrinsic::WritableStdout {
                    ("wasi:cli/stdout@0.3.0", 0)
                } else {
                    ("wasi:cli/stderr@0.3.0", 1)
                };
                self.needs_import(interface, "Writable.toWeb")?;
                self.state.uses_io = true;
                let fd = self.i32_const(ctx, fd);
                Some(self.runtime(ctx, Runtime::WebWritableNew, vec![fd]))
            }
            Intrinsic::WriterWrite => {
                let leased = self.i32_const(ctx, i32::from(self.state.leased.contains(&op)));
                Some(self.runtime(ctx, Runtime::WebWriterWrite, vec![args[0], args[1], leased]))
            }
            Intrinsic::Fetch => {
                self.needs_import("wasi:http/client@0.3.0", "fetch")?;
                self.needs_import("wasi:http/types@0.3.0", "fetch")?;
                self.state.uses_fetch = true;
                self.state.uses_streams = true;
                Some(self.runtime(ctx, Runtime::WebFetch, args))
            }
            Intrinsic::ServeRequest => {
                self.needs_import("wasi:http/types@0.3.0", "an HTTP handler")?;
                self.state.uses_http_server = true;
                self.state.uses_streams = true;
                Some(self.runtime(ctx, Runtime::WebServeRequest, args))
            }
            Intrinsic::ResponseJson | Intrinsic::RequestJson => {
                self.state.uses_json_bodies = true;
                Some(self.runtime(ctx, Runtime::WebBodyJson, args))
            }
            Intrinsic::ResponseBody | Intrinsic::RequestBody => {
                self.state.uses_streams = true;
                Some(self.runtime(ctx, Runtime::WebBodyStream, args))
            }
            Intrinsic::ReaderReadByob => {
                self.state.uses_streams = true;
                Some(self.runtime(ctx, Runtime::WebReaderReadByob, args))
            }
            Intrinsic::ReaderRead => {
                self.state.uses_streams = true;
                let leased = self.i32_const(ctx, i32::from(self.state.leased.contains(&op)));
                Some(self.runtime(ctx, Runtime::WebReaderRead, vec![args[0], leased]))
            }
            Intrinsic::FsReadText
            | Intrinsic::FsReadBytes
            | Intrinsic::FsWriteText
            | Intrinsic::FsWriteBytes
            | Intrinsic::FsStat
            | Intrinsic::FsReaddir
            | Intrinsic::FsMkdir
            | Intrinsic::FsUnlink
            | Intrinsic::FsRmdir => {
                self.needs_import("wasi:filesystem/types@0.3.0", "node:fs")?;
                self.needs_import("wasi:filesystem/preopens@0.3.0", "node:fs")?;
                self.state.uses_fs = true;
                let function = match intrinsic {
                    Intrinsic::FsReadText => Runtime::FsReadText,
                    Intrinsic::FsReadBytes => Runtime::FsReadBytes,
                    Intrinsic::FsWriteText => Runtime::FsWriteText,
                    Intrinsic::FsWriteBytes => Runtime::FsWriteBytes,
                    Intrinsic::FsStat => Runtime::FsStat,
                    Intrinsic::FsReaddir => Runtime::FsReaddir,
                    Intrinsic::FsMkdir => Runtime::FsMkdir,
                    Intrinsic::FsUnlink => Runtime::FsUnlink,
                    _ => Runtime::FsRmdir,
                };
                Some(self.runtime(ctx, function, args))
            }
            other => {
                let function = match other {
                    Intrinsic::StringScalarConcat => Runtime::StringScalarConcat,
                    Intrinsic::StringScalarAppend => Runtime::StringScalarAppend,
                    Intrinsic::BigIntFromNumber => Runtime::StdBigintFromNumber,
                    Intrinsic::BigIntNegate => Runtime::StdBigintNegate,
                    Intrinsic::BigIntToNumber => Runtime::StdBigintToNumber,
                    Intrinsic::BigIntToString => Runtime::StdBigintToString,
                    Intrinsic::StringFromCodePoint => Runtime::StdStringFromCodePoint,
                    Intrinsic::StringCodePointAt => Runtime::StdStringCodePointAt,
                    Intrinsic::StringIndex => Runtime::StdStringIndex,
                    Intrinsic::StringSlice => Runtime::StdStringSlice,
                    Intrinsic::StringSubstring => Runtime::StdStringSubstring,
                    Intrinsic::StringIndexOf => Runtime::StdStringIndexOf,
                    Intrinsic::StringLastIndexOf => Runtime::StdStringLastIndexOf,
                    Intrinsic::StringStartsWith => Runtime::StdStringStartsWith,
                    Intrinsic::StringEndsWith => Runtime::StdStringEndsWith,
                    Intrinsic::StringTrim => Runtime::StdStringTrim,
                    Intrinsic::StringSplit => Runtime::StdStringSplit,
                    Intrinsic::StringRepeat => Runtime::StdStringRepeat,
                    Intrinsic::BytesNew => Runtime::StdBytesNew,
                    Intrinsic::BytesFromArray => Runtime::StdBytesFromArray,
                    Intrinsic::BytesBuffer => Runtime::StdBytesBuffer,
                    Intrinsic::BytesOffset => Runtime::StdBytesOffset,
                    Intrinsic::BufferLength => Runtime::StdBufferLength,
                    Intrinsic::BytesFromBuffer => Runtime::StdBytesFromBuffer,
                    Intrinsic::BytesCopy => Runtime::StdBytesCopy,
                    Intrinsic::BytesGet => Runtime::StdBytesGet,
                    Intrinsic::BytesSet => Runtime::StdBytesSet,
                    Intrinsic::BytesLength => Runtime::StdBytesLength,
                    Intrinsic::BytesSubarray => Runtime::StdBytesSubarray,
                    Intrinsic::BytesSlice => Runtime::StdBytesSlice,
                    Intrinsic::BytesSetFrom => Runtime::StdBytesSetFrom,
                    Intrinsic::BytesFill => Runtime::StdBytesFill,
                    Intrinsic::TextEncode => Runtime::StdTextEncode,
                    Intrinsic::StringByteInput => Runtime::StdStringByteInput,
                    Intrinsic::UrlNew => Runtime::WebUrlNew,
                    Intrinsic::UrlGet => Runtime::WebUrlGet,
                    Intrinsic::UrlSetPathname => Runtime::WebUrlSetPathname,
                    Intrinsic::UrlAppend => Runtime::WebUrlAppend,
                    Intrinsic::EncodeUriComponent => Runtime::WebEncodeUriComponent,
                    Intrinsic::RegexSearch => Runtime::StdRegexSearch,
                    Intrinsic::DecoderValidateLabel => Runtime::StdDecoderValidateLabel,
                    Intrinsic::DecoderNew => Runtime::StdDecoderNew,
                    Intrinsic::TextDecode => Runtime::StdDecoderDecode,
                    Intrinsic::JsonMember => Runtime::StdJsonMember,
                    Intrinsic::JsonKeys => Runtime::StdJsonKeys,
                    Intrinsic::JsonIndex => Runtime::StdJsonIndex,
                    Intrinsic::JsonLength => Runtime::StdJsonLength,
                    Intrinsic::JsonToNumber => Runtime::StdJsonToNumber,
                    Intrinsic::JsonRequireNumber => Runtime::StdJsonRequireNumber,
                    Intrinsic::JsonToString => Runtime::StdJsonToString,
                    Intrinsic::JsonToBool => Runtime::StdJsonToBool,
                    Intrinsic::JsonTruthy => Runtime::StdJsonTruthy,
                    Intrinsic::JsonDisplay => Runtime::StdJsonDisplay,
                    Intrinsic::JsonTypeOf => Runtime::StdJsonTypeOf,
                    Intrinsic::JsonRequireObject => Runtime::StdJsonRequireObject,
                    Intrinsic::JsonFirst => Runtime::StdJsonFirst,
                    Intrinsic::JsonNext => Runtime::StdJsonNext,
                    Intrinsic::JsonIsNull => Runtime::StdJsonIsNull,
                    Intrinsic::JsonObject => Runtime::StdJsonObject,
                    Intrinsic::JsonAssign => Runtime::StdJsonAssign,
                    Intrinsic::JsonSet => Runtime::StdJsonSet,
                    Intrinsic::JsonFromNumber => Runtime::StdJsonFromNumber,
                    Intrinsic::JsonFromString => Runtime::StdJsonFromString,
                    Intrinsic::StatsIsFile => Runtime::FsStatsIsFile,
                    Intrinsic::StatsIsDirectory => Runtime::FsStatsIsDirectory,
                    Intrinsic::StatsSize => Runtime::FsStatsSize,
                    Intrinsic::HeadersNew => Runtime::WebHeadersNew,
                    Intrinsic::HeadersCopy => Runtime::WebHeadersCopy,
                    Intrinsic::HeadersAppend => Runtime::WebHeadersAppend,
                    Intrinsic::HeadersSet => Runtime::WebHeadersSet,
                    Intrinsic::HeadersGet => Runtime::WebHeadersGet,
                    Intrinsic::HeadersHas => Runtime::WebHeadersHas,
                    Intrinsic::HeadersDelete => Runtime::WebHeadersDelete,
                    Intrinsic::ResponseNew => Runtime::WebResponseNew,
                    Intrinsic::ResponseStatus => Runtime::WebResponseStatus,
                    Intrinsic::ResponseStatusText => Runtime::WebResponseStatusText,
                    Intrinsic::ResponseOk => Runtime::WebResponseOk,
                    Intrinsic::ResponseHeaders => Runtime::WebResponseHeaders,
                    Intrinsic::ResponseBodyUsed | Intrinsic::RequestBodyUsed => {
                        Runtime::WebBodyUsed
                    }
                    Intrinsic::ResponseBytes | Intrinsic::RequestBytes => Runtime::WebBodyBytes,
                    Intrinsic::ServeBody => Runtime::WebServeBody,
                    Intrinsic::ServeMethod => Runtime::WebServeMethod,
                    Intrinsic::ServeMethodOther => Runtime::WebServeMethodOther,
                    Intrinsic::ServeHasScheme => Runtime::WebServeHasScheme,
                    Intrinsic::ServeScheme => Runtime::WebServeScheme,
                    Intrinsic::ServeSchemeOther => Runtime::WebServeSchemeOther,
                    Intrinsic::ServeAuthority => Runtime::WebServeAuthority,
                    Intrinsic::ServePath => Runtime::WebServePath,
                    Intrinsic::ServeHeaderCount => Runtime::WebServeHeaderCount,
                    Intrinsic::ServeHeaderName => Runtime::WebServeHeaderName,
                    Intrinsic::ServeHeaderValue => Runtime::WebServeHeaderValue,
                    Intrinsic::ServeAddHeader => Runtime::WebServeAddHeader,
                    Intrinsic::ServeRespond => Runtime::WebServeRespond,
                    Intrinsic::ResponseText | Intrinsic::RequestText => Runtime::WebBodyText,
                    Intrinsic::RequestNew => Runtime::WebRequestNew,
                    Intrinsic::RequestMethod => Runtime::WebRequestMethod,
                    Intrinsic::WritableLocked => Runtime::WebWritableLocked,
                    Intrinsic::WritableGetWriter => Runtime::WebWritableGetWriter,
                    Intrinsic::WriterClose => Runtime::WebWriterClose,
                    Intrinsic::WriterReleaseLock => Runtime::WebWriterReleaseLock,
                    Intrinsic::ReadableLocked => Runtime::WebReadableLocked,
                    Intrinsic::ReadableGetReader | Intrinsic::ReadableGetByobReader => {
                        Runtime::WebReadableGetReader
                    }
                    Intrinsic::ReadableCancel => Runtime::WebReadableCancel,
                    Intrinsic::ReaderCancel => Runtime::WebReaderCancel,
                    Intrinsic::ReaderReleaseLock => Runtime::WebReaderReleaseLock,
                    Intrinsic::ReadDone => Runtime::WebReadDone,
                    Intrinsic::ReadValue => Runtime::WebReadValue,
                    Intrinsic::RequestUrl => Runtime::WebRequestUrl,
                    Intrinsic::RequestHeaders => Runtime::WebRequestHeaders,
                    Intrinsic::ExceptionToBool => Runtime::StdExceptionToBool,
                    Intrinsic::ExceptionToNumber => Runtime::StdExceptionToNumber,
                    Intrinsic::ExceptionName => Runtime::StdExceptionName,
                    Intrinsic::ExceptionToString => Runtime::StdExceptionToString,
                    other => bail!("intrinsic {} has no runtime lowering", other.name()),
                };
                let result = self.call_runtime(ctx, function, args);
                // Runtime results as `number | undefined` come as bits.
                result.map(|value| {
                    if function.result() == Some(W::I64) {
                        self.num(ctx, NumOpcode::F64ReinterpretI64, vec![value])
                    } else {
                        value
                    }
                })
            }
        };
        if intrinsic.throws() && !matches!(intrinsic, Intrinsic::JsonParse | Intrinsic::DateIso) {
            let status = self.runtime(ctx, Runtime::Status, vec![]);
            self.throw_status_if(ctx, status, status);
        }
        if let Some(mut value) = value {
            let ty = op.deref(ctx).get_result(0).get_type(ctx);
            if absence(ctx, ty) == Some(perry_mir::Absence::Null) && repr(ctx, ty)? == Repr::Ref {
                // Helpers use C null pointers; compiled null has a stable distinct sentinel.
                let absent = self.num(ctx, NumOpcode::I32Eqz, vec![value]);
                let null = self.null(ctx, ty)?;
                value = self.select(ctx, absent, null, value);
            }
            self.define(ctx, op, value);
        }
        Ok(())
    }

    /// Evaluate a `Number.is*` predicate on an exact operand: integer tags are finite and
    /// integral, and only their magnitude decides safety.
    fn number_predicate(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        predicate: NumberPredicate,
    ) -> Result<Value> {
        let (tag, payload) = self.numeric_operand(ctx, op, 0)?;
        let float_tag = self.i32_const(ctx, perry_abi::layout::numeric::FLOAT);
        let f = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![payload]);
        Ok(match predicate {
            NumberPredicate::NaN => {
                let is_float = self.num(ctx, NumOpcode::I32Eq, vec![tag, float_tag]);
                let is_nan = self.num(ctx, NumOpcode::F64Ne, vec![f, f]);
                self.num(ctx, NumOpcode::I32And, vec![is_float, is_nan])
            }
            NumberPredicate::Finite => {
                let not_float = self.num(ctx, NumOpcode::I32Ne, vec![tag, float_tag]);
                let abs = self.num(ctx, NumOpcode::F64Abs, vec![f]);
                let inf = self.f64_const(ctx, f64::INFINITY);
                let finite = self.num(ctx, NumOpcode::F64Lt, vec![abs, inf]);
                self.num(ctx, NumOpcode::I32Or, vec![not_float, finite])
            }
            NumberPredicate::Integer => {
                let not_float = self.num(ctx, NumOpcode::I32Ne, vec![tag, float_tag]);
                let abs = self.num(ctx, NumOpcode::F64Abs, vec![f]);
                let inf = self.f64_const(ctx, f64::INFINITY);
                let finite = self.num(ctx, NumOpcode::F64Lt, vec![abs, inf]);
                let trunc = self.num(ctx, NumOpcode::F64Trunc, vec![f]);
                let integral = self.num(ctx, NumOpcode::F64Eq, vec![trunc, f]);
                let float_int = self.num(ctx, NumOpcode::I32And, vec![finite, integral]);
                self.num(ctx, NumOpcode::I32Or, vec![not_float, float_int])
            }
            NumberPredicate::SafeInteger => {
                let abs = self.num(ctx, NumOpcode::F64Abs, vec![f]);
                let max_safe = self.f64_const(ctx, 9_007_199_254_740_991.0);
                let in_range = self.num(ctx, NumOpcode::F64Le, vec![abs, max_safe]);
                let trunc = self.num(ctx, NumOpcode::F64Trunc, vec![f]);
                let integral = self.num(ctx, NumOpcode::F64Eq, vec![trunc, f]);
                let float_safe = self.num(ctx, NumOpcode::I32And, vec![in_range, integral]);
                let min_i64 = self.i64_const(ctx, (-9_007_199_254_740_991i64) as u64);
                let max_i64 = self.i64_const(ctx, 9_007_199_254_740_991);
                let ge_min = self.num(ctx, NumOpcode::I64GeS, vec![payload, min_i64]);
                let le_max = self.num(ctx, NumOpcode::I64LeS, vec![payload, max_i64]);
                let signed_safe = self.num(ctx, NumOpcode::I32And, vec![ge_min, le_max]);
                let unsigned_safe = self.num(ctx, NumOpcode::I64LeU, vec![payload, max_i64]);
                let one = self.i32_const(ctx, 1);
                let is_signed = self.num(ctx, NumOpcode::I32And, vec![tag, one]);
                let int_safe = self.select(ctx, is_signed, signed_safe, unsigned_safe);
                let is_float = self.num(ctx, NumOpcode::I32Eq, vec![tag, float_tag]);
                self.select(ctx, is_float, float_safe, int_safe)
            }
        })
    }
}

/// The `Number.is*` predicates lowered exactly on integer and tagged operands.
#[derive(Clone, Copy)]
enum NumberPredicate {
    NaN,
    Finite,
    Integer,
    SafeInteger,
}
