//! Runtime and host intrinsic lowering.
use crate::strings;
use crate::{
    MONOTONIC_CLOCK,
    function::FunctionLowering,
    repr::{Carriers, Repr, repr},
    runtime::{Runtime, W},
};
use anyhow::{Context as _, Result, bail, ensure};
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
            let out_ptr = self.numeric_result_slot(ctx);
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
        if intrinsic == Intrinsic::NumberInspect
            || (intrinsic == Intrinsic::NumberToString && exact_number(ctx))
        {
            let (tag, payload) = self.numeric_operand(ctx, op, 0)?;
            let norm_tag = self.canonical_numeric_tag(ctx, tag);
            let runtime = if intrinsic == Intrinsic::NumberInspect {
                Runtime::StdNumberInspect
            } else {
                Runtime::StdNumberToString
            };
            let value = self.runtime(ctx, runtime, vec![norm_tag, payload]);
            self.define(ctx, op, value);
            return Ok(());
        }
        if intrinsic == Intrinsic::NumberToStringRadix {
            let (tag, payload) = self.numeric_operand(ctx, op, 0)?;
            let norm_tag = self.canonical_numeric_tag(ctx, tag);
            let radix_op = op.deref(ctx).get_operand(1);
            let radix = self.mapped(ctx, radix_op)?;
            let value = self.runtime(
                ctx,
                Runtime::StdNumberToStringRadix,
                vec![norm_tag, payload, radix],
            );
            let status = self.runtime(ctx, Runtime::Status, vec![]);
            self.throw_status_if(ctx, status, status);
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
            Intrinsic::JsonSpread | Intrinsic::JsonOrderCopy => {
                let target_ty = op.deref(ctx).get_operand(0).get_type(ctx);
                let source_ty = op.deref(ctx).get_operand(1).get_type(ctx);
                let (table, count) = self.json_field_table(ctx, target_ty)?;
                let (source_table, source_count) = self.json_field_table(ctx, source_ty)?;
                let table = self.i32_const(ctx, table as i32);
                let count = self.i32_const(ctx, count as i32);
                let source_table = self.i32_const(ctx, source_table as i32);
                let source_count = self.i32_const(ctx, source_count as i32);
                self.runtime_void(
                    ctx,
                    Runtime::StdJsonRecordSpread,
                    vec![args[0], args[1], table, count, source_table, source_count],
                );
                if intrinsic == Intrinsic::JsonOrderCopy {
                    let record = TypedHandle::<perry_mir::RecordType>::from_handle(target_ty, ctx)?;
                    let fields = record.deref(ctx).fields()?.to_vec();
                    for (name, ty) in fields {
                        if absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
                            let (offset, _) = Self::record_type_field_slot(ctx, record, &name)?;
                            let value = self.load_slot_value(ctx, args[0], offset, ty)?;
                            let omitted = self.is_omitted(ctx, value, ty)?;
                            let present = self.num(ctx, NumOpcode::I32Eqz, vec![omitted]);
                            let key = self.state.data.string(&name);
                            let key = self.i32_const(ctx, key as i32);
                            self.runtime_void(
                                ctx,
                                Runtime::StdJsonRecordField,
                                vec![args[0], table, count, key, present],
                            );
                        }
                    }
                }
                None
            }
            Intrinsic::JsonInsert | Intrinsic::JsonExclude => {
                let ty = op.deref(ctx).get_operand(0).get_type(ctx);
                let (table, count) = self.json_field_table(ctx, ty)?;
                let table = self.i32_const(ctx, table as i32);
                let count = self.i32_const(ctx, count as i32);
                let present = self.i32_const(ctx, i32::from(intrinsic == Intrinsic::JsonInsert));
                self.runtime_void(
                    ctx,
                    Runtime::StdJsonRecordField,
                    vec![args[0], table, count, args[1], present],
                );
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
            | Intrinsic::PlainDateFromFields
            | Intrinsic::PlainDateTimeFrom
            | Intrinsic::PlainDateAddDays
            | Intrinsic::PlainDateAddCalendar
            | Intrinsic::PlainDateTimeAddDays
            | Intrinsic::PlainDateTimeAddCalendar
            | Intrinsic::PlainDateEpochDays
            | Intrinsic::PlainDateUntilPart
            | Intrinsic::InstantMs
            | Intrinsic::PlainDatePart
            | Intrinsic::PlainDateTimePart
            | Intrinsic::InstantString
            | Intrinsic::PlainDateString
            | Intrinsic::PlainDateTimeString => {
                Some(self.temporal_intrinsic(ctx, intrinsic, args)?)
            }
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
            Intrinsic::ReaderRead => {
                self.state.uses_streams = true;
                let leased = self.i32_const(ctx, i32::from(self.state.leased.contains(&op)));
                Some(self.runtime(ctx, Runtime::WebReaderRead, vec![args[0], leased]))
            }
            other => {
                match other {
                    Intrinsic::EnvGet | Intrinsic::EnvKeys => {
                        self.needs_import("wasi:cli/environment@0.3.0", "process.env")?;
                    }
                    Intrinsic::PerformanceNow => {
                        self.needs_import(MONOTONIC_CLOCK, "performance.now()")?;
                    }
                    Intrinsic::DateNow => {
                        self.needs_import("wasi:clocks/system-clock@0.3.0", "Date.now()")?;
                    }
                    Intrinsic::MathRandom => {
                        self.needs_import("wasi:random/insecure@0.3.0", "Math.random()")?;
                    }
                    Intrinsic::RandomUuid => {
                        self.needs_import("wasi:random/random@0.3.0", "crypto.randomUUID()")?;
                    }
                    Intrinsic::BytesFillRandom => {
                        self.needs_import("wasi:random/random@0.3.0", "crypto.getRandomValues()")?;
                    }
                    Intrinsic::Fetch => {
                        self.needs_import("wasi:http/client@0.3.0", "fetch")?;
                        self.needs_import("wasi:http/types@0.3.0", "fetch")?;
                        self.state.uses_fetch = true;
                        self.state.uses_streams = true;
                    }
                    Intrinsic::ServeRequest => {
                        self.needs_import("wasi:http/types@0.3.0", "an HTTP handler")?;
                        self.state.uses_http_server = true;
                        self.state.uses_streams = true;
                    }
                    Intrinsic::ResponseJson | Intrinsic::RequestJson => {
                        self.state.uses_json_bodies = true;
                    }
                    Intrinsic::ResponseBody
                    | Intrinsic::RequestBody
                    | Intrinsic::ReaderReadByob => {
                        self.state.uses_streams = true;
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
                    }
                    _ => {}
                }
                let function = Runtime::for_intrinsic(other).with_context(|| {
                    format!("intrinsic {} has no runtime lowering", other.name())
                })?;
                let result = self.call_runtime(ctx, function, args);
                // Runtime results as `number | undefined` come as bits.
                result.map(|value| {
                    if function.result() == Some(W::F64Bits) {
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
