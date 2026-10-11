//! Boxed UTC dates and their calendar helper calls.
use crate::{function::FunctionLowering, repr::i32_type, runtime::Runtime};
use perry_mir::intrinsics::Intrinsic;
use pliron::{context::Context, value::Value};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_, '_> {
    pub(super) fn date_intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Option<Value> {
        match intrinsic {
            Intrinsic::DateFromTime => {
                // TimeClip: NaN beyond ±8.64e15 ms, else the integer part, without -0.
                let t = args[0];
                let magnitude = self.num(ctx, NumOpcode::F64Abs, vec![t]);
                let limit = self.f64_const(ctx, 8.64e15);
                let valid = self.num(ctx, NumOpcode::F64Le, vec![magnitude, limit]);
                let truncated = self.num(ctx, NumOpcode::F64Trunc, vec![t]);
                let zero = self.f64_const(ctx, 0.0);
                let clipped = self.num(ctx, NumOpcode::F64Add, vec![truncated, zero]);
                let nan = self.f64_const(ctx, f64::NAN);
                let time = self.select(ctx, valid, clipped, nan);
                let size = self.i32_const(ctx, 8);
                let date = self.alloc(ctx, size);
                self.store(ctx, MemKind::F64, date, 0, time);
                Some(date)
            }
            Intrinsic::DateTime => Some(self.load(ctx, MemKind::F64, args[0], 0)),
            Intrinsic::DateParse => {
                let data = self.load(ctx, MemKind::I32, args[0], perry_runtime::string::DATA);
                let len = self.load(ctx, MemKind::I32, args[0], perry_runtime::string::LEN);
                Some(self.runtime(ctx, Runtime::TimeDateParse, vec![data, len]))
            }
            Intrinsic::DateUtc => Some(self.runtime(ctx, Runtime::TimeDateUtc, args)),
            Intrinsic::DatePart => {
                let time = self.load(ctx, MemKind::F64, args[0], 0);
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[1]]);
                Some(self.runtime(ctx, Runtime::TimeDatePart, vec![time, part]))
            }
            Intrinsic::DateSet => {
                let date = args[0];
                let time = self.load(ctx, MemKind::F64, date, 0);
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[1]]);
                let count = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[2]]);
                let mut values = vec![time, part, count];
                values.extend_from_slice(&args[3..]);
                let updated = self.runtime(ctx, Runtime::TimeDateSet, values);
                self.store(ctx, MemKind::F64, date, 0, updated);
                Some(updated)
            }
            Intrinsic::DateIso | Intrinsic::DateJson => {
                let size = self.i32_const(ctx, 32);
                let out = self.alloc(ctx, size);
                let capacity = self.i32_const(ctx, 32);
                let time = self.load(ctx, MemKind::F64, args[0], 0);
                let packed = self.runtime(ctx, Runtime::TimeDateIso, vec![time, out, capacity]);
                let thirty_two = self.i64_const(ctx, 32);
                let error = self.num(ctx, NumOpcode::I64ShrU, vec![packed, thirty_two]);
                let error = self.num(ctx, NumOpcode::I32WrapI64, vec![error]);
                let failed = self.new_block(ctx, vec![]);
                let formatted = self.new_block(ctx, vec![]);
                self.cond_br(ctx, error, (failed, vec![]), (formatted, vec![]));
                self.block = failed;
                let merged_type = i32_type(ctx);
                let merged = self.new_block(ctx, vec![merged_type]);
                if intrinsic == Intrinsic::DateIso {
                    let status = self.runtime(ctx, Runtime::StdRaiseInvalidDate, vec![]);
                    let (target, throw_args) = self.throw_target(ctx, status);
                    self.br(ctx, target, throw_args);
                } else {
                    let null = self.i32_const(ctx, 1);
                    self.br(ctx, merged, vec![null]);
                }
                self.block = formatted;
                let len = self.num(ctx, NumOpcode::I32WrapI64, vec![packed]);
                let text = self.runtime(ctx, Runtime::StringAdopt, vec![out, len]);
                self.br(ctx, merged, vec![text]);
                self.block = merged;
                Some(merged.deref(ctx).get_argument(0))
            }
            _ => unreachable!("not a date intrinsic"),
        }
    }
}
