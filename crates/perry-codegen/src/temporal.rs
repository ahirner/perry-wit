//! Temporal codec calls with opaque collectible payloads.
use crate::{function::FunctionLowering, runtime::Runtime};
use perry_mir::intrinsics::Intrinsic;
use pliron::{context::Context, value::Value};
use pliron_wasm::attrs::{MemKind, NumOpcode};

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
        capacity: i32,
    ) -> (Value, Value) {
        let size = self.i32_const(ctx, capacity);
        let out = self.alloc(ctx, size);
        args.push(out);
        if capacity != 16 {
            args.push(size);
        }
        let packed = self.runtime(ctx, function, args);
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
        let len = self.num(ctx, NumOpcode::I32WrapI64, vec![packed]);
        (out, len)
    }

    /// A new 16-byte `Temporal` value from a codec (`signature` ends with the output).
    fn time_value(&mut self, ctx: &mut Context, function: Runtime, args: Vec<Value>) -> Value {
        self.time_codec(ctx, function, args, 16).0
    }

    /// The ISO string of a `Temporal` value (at most 40 bytes: a six-digit signed year and
    /// nanoseconds).
    fn time_string(&mut self, ctx: &mut Context, function: Runtime, value: Value) -> Value {
        let (out, len) = self.time_codec(ctx, function, vec![value], 48);
        self.runtime(ctx, Runtime::StringAdopt, vec![out, len])
    }

    pub(super) fn temporal_intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Option<Value> {
        match intrinsic {
            Intrinsic::InstantFromMs => {
                Some(self.time_value(ctx, Runtime::TimeInstantFromMs, args))
            }
            Intrinsic::InstantFrom => {
                let args = self.string_parts(ctx, args[0]);
                Some(self.time_value(ctx, Runtime::TimeInstantParse, args))
            }
            Intrinsic::PlainDateFrom => {
                let args = self.string_parts(ctx, args[0]);
                Some(self.time_value(ctx, Runtime::TimePlainDateParse, args))
            }
            Intrinsic::PlainDateTimeFrom => {
                let args = self.string_parts(ctx, args[0]);
                Some(self.time_value(ctx, Runtime::TimePlainParse, args))
            }
            Intrinsic::PlainDateAddDays | Intrinsic::PlainDateTimeAddDays => {
                Some(self.time_value(ctx, Runtime::TimePlainAddDays, args))
            }
            Intrinsic::InstantMs => Some(self.runtime(ctx, Runtime::TimeInstantMs, args)),
            Intrinsic::PlainDatePart | Intrinsic::PlainDateTimePart => {
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[1]]);
                Some(self.runtime(ctx, Runtime::TimePlainPart, vec![args[0], part]))
            }
            Intrinsic::InstantString => {
                Some(self.time_string(ctx, Runtime::TimeInstantFormat, args[0]))
            }
            Intrinsic::PlainDateString => {
                Some(self.time_string(ctx, Runtime::TimePlainDateFormat, args[0]))
            }
            Intrinsic::PlainDateTimeString => {
                Some(self.time_string(ctx, Runtime::TimePlainFormat, args[0]))
            }
            _ => unreachable!("not a Temporal intrinsic"),
        }
    }
}
