//! Temporal codec calls with opaque collectible payloads.
use crate::{function::FunctionLowering, runtime::Runtime};
use anyhow::{Result, bail};
use perry_mir::Intrinsic;
use pliron::{context::Context, value::Value};
use pliron_wasm::attrs::{MemKind, NumOpcode};

enum TimeOutput {
    Value,
    Text,
}

impl FunctionLowering<'_, '_> {
    /// A string's UTF-8 bytes: `[data, len]`.
    fn string_parts(&mut self, ctx: &mut Context, string: Value) -> Vec<Value> {
        use perry_abi::layout::string::{DATA, LEN};
        let data = self.load(ctx, MemKind::I32, string, DATA);
        let len = self.load(ctx, MemKind::I32, string, LEN);
        vec![data, len]
    }

    /// Call a `time-helper` codec that writes into `capacity` new opaque bytes (passed
    /// after `args`, with the capacity for text results) and returns
    /// `status << 32 | length`; a failure throws `RangeError`. Returns the bytes and length.
    fn time_codec(
        &mut self,
        ctx: &mut Context,
        function: Runtime,
        mut args: Vec<Value>,
        output: TimeOutput,
    ) -> (Value, Value) {
        let capacity = match output {
            TimeOutput::Value => 16,
            TimeOutput::Text => 48,
        };
        let size = self.i32_const(ctx, capacity);
        let out = self.alloc(ctx, size);
        args.push(out);
        if matches!(output, TimeOutput::Text) {
            args.push(size);
        }
        let packed = self.runtime(ctx, function, args);
        self.check_time_status(ctx, packed);
        let len = self.num(ctx, NumOpcode::I32WrapI64, vec![packed]);
        (out, len)
    }

    fn check_time_status(&mut self, ctx: &mut Context, packed: Value) {
        let thirty_two = self.i64_const(ctx, 32);
        let status = self.num(ctx, NumOpcode::I64ShrU, vec![packed, thirty_two]);
        let status = self.num(ctx, NumOpcode::I32WrapI64, vec![status]);
        let failed = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.cond_br(ctx, status, (failed, vec![]), (done, vec![]));
        self.block = failed;
        let message = self.state.data.string("Invalid time value for Temporal");
        let message = self.i64_const(ctx, u64::from(message));
        let kind = self.i32_const(ctx, perry_abi::layout::exception::RANGE_ERROR as i32);
        let tag = self.i32_const(ctx, perry_abi::layout::exception::TAG_STRING as i32);
        let exception = self.runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, message]);
        let status = self.runtime(ctx, Runtime::ExceptionRaise, vec![exception]);
        let (target, throw_args) = self.throw_target(ctx, status);
        self.br(ctx, target, throw_args);
        self.block = done;
    }

    fn time_scalar(&mut self, ctx: &mut Context, function: Runtime, mut args: Vec<Value>) -> Value {
        let out = self.numeric_result_slot(ctx);
        args.push(out);
        let packed = self.runtime(ctx, function, args);
        self.check_time_status(ctx, packed);
        self.load(ctx, MemKind::F64, out, 0)
    }

    /// A new 16-byte `Temporal` value from a codec (`signature` ends with the output).
    fn time_value(&mut self, ctx: &mut Context, function: Runtime, args: Vec<Value>) -> Value {
        self.time_codec(ctx, function, args, TimeOutput::Value).0
    }

    /// The ISO string of a `Temporal` value (at most 40 bytes: a six-digit signed year and
    /// nanoseconds).
    fn time_string(&mut self, ctx: &mut Context, function: Runtime, value: Value) -> Value {
        let (out, len) = self.time_codec(ctx, function, vec![value], TimeOutput::Text);
        self.runtime(ctx, Runtime::StringAdopt, vec![out, len])
    }

    pub(super) fn temporal_intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Result<Value> {
        Ok(match intrinsic {
            Intrinsic::InstantFromMs => self.time_value(ctx, Runtime::TimeInstantFromMs, args),
            Intrinsic::InstantFrom => {
                let args = self.string_parts(ctx, args[0]);
                self.time_value(ctx, Runtime::TimeInstantParse, args)
            }
            Intrinsic::PlainDateFrom => {
                let args = self.string_parts(ctx, args[0]);
                self.time_value(ctx, Runtime::TimePlainDateParse, args)
            }
            Intrinsic::PlainDateFromFields => {
                let constrain = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[3]]);
                self.time_value(
                    ctx,
                    Runtime::TimePlainDateFromFields,
                    vec![args[0], args[1], args[2], constrain],
                )
            }
            Intrinsic::PlainDateTimeFrom => {
                let args = self.string_parts(ctx, args[0]);
                self.time_value(ctx, Runtime::TimePlainParse, args)
            }
            Intrinsic::PlainDateAddDays | Intrinsic::PlainDateTimeAddDays => {
                self.time_value(ctx, Runtime::TimePlainAddDays, args)
            }
            Intrinsic::PlainDateAddCalendar | Intrinsic::PlainDateTimeAddCalendar => {
                let constrain = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[5]]);
                self.time_value(
                    ctx,
                    Runtime::TimePlainAddCalendar,
                    vec![args[0], args[1], args[2], args[3], args[4], constrain],
                )
            }
            Intrinsic::PlainDateEpochDays => {
                let day = self.load(ctx, MemKind::I32, args[0], 0);
                self.num(ctx, NumOpcode::F64ConvertI32S, vec![day])
            }
            Intrinsic::PlainDateUntilPart => {
                let unit = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[2]]);
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[3]]);
                self.time_scalar(
                    ctx,
                    Runtime::TimePlainUntilPart,
                    vec![args[0], args[1], unit, part],
                )
            }
            Intrinsic::InstantMs => self.time_scalar(ctx, Runtime::TimeInstantMs, args),
            Intrinsic::PlainDatePart | Intrinsic::PlainDateTimePart => {
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[1]]);
                self.time_scalar(ctx, Runtime::TimePlainPart, vec![args[0], part])
            }
            Intrinsic::InstantString => self.time_string(ctx, Runtime::TimeInstantFormat, args[0]),
            Intrinsic::PlainDateString => {
                self.time_string(ctx, Runtime::TimePlainDateFormat, args[0])
            }
            Intrinsic::PlainDateTimeString => {
                self.time_string(ctx, Runtime::TimePlainFormat, args[0])
            }
            _ => bail!("not a Temporal intrinsic: {intrinsic:?}"),
        })
    }
}
