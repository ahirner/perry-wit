//! Temporal codec calls with opaque collectible payloads.
use super::*;

impl FunctionLowering<'_> {
    /// A string's UTF-8 bytes: `[data, len]`.
    fn string_parts(&mut self, ctx: &mut Context, string: Value) -> Vec<Value> {
        use perry_runtime::string::{DATA, LEN};
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
        name: &'static str,
        signature: &'static [W],
        mut args: Vec<Value>,
        capacity: i32,
    ) -> (Value, Value) {
        let size = self.i32_const(ctx, capacity);
        let out = self.runtime(ctx, Runtime::Alloc, vec![size]).expect("out");
        args.push(out);
        if capacity != 16 {
            args.push(size);
        }
        let packed = self
            .call(ctx, name, signature, &[W::I64], args)
            .expect("result");
        let thirty_two = self.i64_const(ctx, 32);
        let status = self.num(ctx, NumOpcode::I64ShrU, vec![packed, thirty_two]);
        let status = self.num(ctx, NumOpcode::I32WrapI64, vec![status]);
        let failed = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.cond_br(ctx, status, (failed, vec![]), (done, vec![]));
        self.block = failed;
        let message = self.state.data.string("Invalid time value for Temporal");
        let message = self.i64_const(ctx, u64::from(message));
        let kind = self.i32_const(ctx, perry_runtime::exception::RANGE_ERROR as i32);
        let tag = self.i32_const(ctx, perry_runtime::exception::TAG_STRING as i32);
        let exception = self
            .runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, message])
            .expect("exception");
        let status = self
            .runtime(ctx, Runtime::ExceptionRaise, vec![exception])
            .expect("status");
        let (target, throw_args) = self.throw_target(ctx, status);
        self.br(ctx, target, throw_args);
        self.block = done;
        let len = self.num(ctx, NumOpcode::I32WrapI64, vec![packed]);
        (out, len)
    }

    /// A new 16-byte `Temporal` value from a codec (`signature` ends with the output).
    fn time_value(
        &mut self,
        ctx: &mut Context,
        name: &'static str,
        signature: &'static [W],
        args: Vec<Value>,
    ) -> Value {
        self.time_codec(ctx, name, signature, args, 16).0
    }

    /// The ISO string of a `Temporal` value (at most 40 bytes: a six-digit signed year and
    /// nanoseconds).
    fn time_string(&mut self, ctx: &mut Context, name: &'static str, value: Value) -> Value {
        let (out, len) = self.time_codec(ctx, name, &[W::I32; 3], vec![value], 48);
        self.runtime(ctx, Runtime::StringAdopt, vec![out, len])
            .expect("string")
    }

    pub(super) fn temporal_intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Option<Value> {
        match intrinsic {
            Intrinsic::InstantFromMs => {
                Some(self.time_value(ctx, "time_instant_from_ms", &[W::F64, W::I32], args))
            }
            Intrinsic::InstantFrom => {
                let args = self.string_parts(ctx, args[0]);
                Some(self.time_value(ctx, "time_instant_parse", &[W::I32; 3], args))
            }
            Intrinsic::PlainDateFrom => {
                let args = self.string_parts(ctx, args[0]);
                Some(self.time_value(ctx, "time_plain_date_parse", &[W::I32; 3], args))
            }
            Intrinsic::PlainDateTimeFrom => {
                let args = self.string_parts(ctx, args[0]);
                Some(self.time_value(ctx, "time_plain_parse", &[W::I32; 3], args))
            }
            Intrinsic::PlainDateAddDays | Intrinsic::PlainDateTimeAddDays => {
                Some(self.time_value(ctx, "time_plain_add_days", &[W::I32, W::F64, W::I32], args))
            }
            Intrinsic::InstantMs => self.call(ctx, "time_instant_ms", &[W::I32], &[W::F64], args),
            Intrinsic::PlainDatePart | Intrinsic::PlainDateTimePart => {
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[1]]);
                let sig: (&[W], &[W]) = (&[W::I32, W::I32], &[W::F64]);
                self.call(ctx, "time_plain_part", sig.0, sig.1, vec![args[0], part])
            }
            Intrinsic::InstantString => Some(self.time_string(ctx, "time_instant_format", args[0])),
            Intrinsic::PlainDateString => {
                Some(self.time_string(ctx, "time_plain_date_format", args[0]))
            }
            Intrinsic::PlainDateTimeString => {
                Some(self.time_string(ctx, "time_plain_format", args[0]))
            }
            _ => unreachable!("not a Temporal intrinsic"),
        }
    }
}
