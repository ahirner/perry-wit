//! Boxed UTC dates and their calendar helper calls.
use super::*;

impl FunctionLowering<'_> {
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
                let date = self.runtime(ctx, Runtime::Alloc, vec![size]).expect("date");
                self.store(ctx, MemKind::F64, date, 0, time);
                Some(date)
            }
            Intrinsic::DateTime => Some(self.load(ctx, MemKind::F64, args[0], 0)),
            Intrinsic::DateParse => {
                let data = self.load(ctx, MemKind::I32, args[0], perry_runtime::string::DATA);
                let len = self.load(ctx, MemKind::I32, args[0], perry_runtime::string::LEN);
                self.call(
                    ctx,
                    "time_date_parse",
                    &[W::I32, W::I32],
                    &[W::F64],
                    vec![data, len],
                )
            }
            Intrinsic::DateUtc => self.call(ctx, "time_date_utc", &[W::F64; 7], &[W::F64], args),
            Intrinsic::DatePart => {
                let time = self.load(ctx, MemKind::F64, args[0], 0);
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[1]]);
                self.call(
                    ctx,
                    "time_date_part",
                    &[W::F64, W::I32],
                    &[W::F64],
                    vec![time, part],
                )
            }
            Intrinsic::DateSet => {
                let date = args[0];
                let time = self.load(ctx, MemKind::F64, date, 0);
                let part = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[1]]);
                let count = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![args[2]]);
                let mut values = vec![time, part, count];
                values.extend_from_slice(&args[3..]);
                let updated = self
                    .call(
                        ctx,
                        "time_date_set",
                        &[W::F64, W::I32, W::I32, W::F64, W::F64, W::F64, W::F64],
                        &[W::F64],
                        values,
                    )
                    .expect("time");
                self.store(ctx, MemKind::F64, date, 0, updated);
                Some(updated)
            }
            Intrinsic::DateIso | Intrinsic::DateJson => {
                let size = self.i32_const(ctx, 32);
                let out = self.runtime(ctx, Runtime::Alloc, vec![size]).expect("out");
                let capacity = self.i32_const(ctx, 32);
                let time = self.load(ctx, MemKind::F64, args[0], 0);
                let packed = self
                    .call(
                        ctx,
                        "time_date_iso",
                        &[W::F64, W::I32, W::I32],
                        &[W::I64],
                        vec![time, out, capacity],
                    )
                    .expect("result");
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
                    let status = self
                        .call(ctx, "std_raise_invalid_date", &[], &[W::I32], vec![])
                        .expect("status");
                    let (target, throw_args) = self.throw_target(ctx, status);
                    self.br(ctx, target, throw_args);
                } else {
                    let null = self.i32_const(ctx, 1);
                    self.br(ctx, merged, vec![null]);
                }
                self.block = formatted;
                let len = self.num(ctx, NumOpcode::I32WrapI64, vec![packed]);
                let text = self
                    .runtime(ctx, Runtime::StringAdopt, vec![out, len])
                    .expect("text");
                self.br(ctx, merged, vec![text]);
                self.block = merged;
                Some(merged.deref(ctx).get_argument(0))
            }
            _ => unreachable!("not a date intrinsic"),
        }
    }
}
