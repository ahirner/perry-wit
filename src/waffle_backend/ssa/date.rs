//! Date construction and UTC operations over clipped epoch milliseconds.

use super::FunctionLowerer;
use crate::waffle_backend::{abi, date::is_date};
use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn new_date(&mut self, arguments: &[Expr]) -> Result<Value> {
        ensure!(
            arguments.len() == 1,
            "Date construction requires one argument"
        );
        let argument = &arguments[0];
        let ty = self.infer_expr_type(argument);
        let helpers = self
            .registry
            .date_helpers
            .expect("Date helpers are registered");
        let time = if ty == HirType::Number {
            self.expression(argument)?
        } else if crate::waffle_backend::values::is_string_type(&ty) {
            let str_val = self.expression(argument)?;
            let ptr = self.op(
                Operator::I32Load {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        align: 2,
                        offset: 0,
                    },
                },
                &[str_val],
                &[Type::I32],
            );
            let len = self.op(
                Operator::I32Load {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        align: 2,
                        offset: 4,
                    },
                },
                &[str_val],
                &[Type::I32],
            );
            self.op(
                Operator::Call {
                    function_index: helpers.parse,
                },
                &[ptr, len],
                &[Type::F64],
            )
        } else {
            bail!("Date construction requires a statically known number or string; found {ty:?}")
        };
        Ok(self.op(
            Operator::Call {
                function_index: helpers.new,
            },
            &[time],
            &[Type::I32],
        ))
    }

    pub(super) fn date_method(
        &mut self,
        receiver: &Expr,
        method: &str,
        arguments: &[Expr],
    ) -> Result<Value> {
        ensure!(
            is_date(&self.infer_expr_type(receiver)),
            "Expected a Date receiver"
        );
        let date = self.expression(receiver)?;
        let helpers = self
            .registry
            .date_helpers
            .expect("Date helpers are registered");

        if method == "getTime" {
            ensure!(arguments.is_empty(), "Date.getTime accepts no arguments");
            return Ok(self.op(
                Operator::F64Load {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        align: 3,
                        offset: 0,
                    },
                },
                &[date],
                &[Type::F64],
            ));
        }

        if method == "toISOString" {
            ensure!(
                arguments.is_empty(),
                "Date.toISOString accepts no arguments"
            );
            let payload = self.call_completion(helpers.iso, &[date]);
            return Ok(abi::decode_payload(
                &mut self.body,
                self.block,
                payload,
                true,
            ));
        }

        if method == "getUTCFullYear"
            || method == "getUTCMonth"
            || method == "getUTCDate"
            || method == "getUTCDay"
        {
            ensure!(arguments.is_empty(), "Date.{method} accepts no arguments");
            let time = self.op(
                Operator::F64Load {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        align: 3,
                        offset: 0,
                    },
                },
                &[date],
                &[Type::F64],
            );
            let part_code = match method {
                "getUTCFullYear" => 0,
                "getUTCMonth" => 1,
                "getUTCDate" => 2,
                "getUTCDay" => 3,
                _ => unreachable!(),
            };
            let part_val = self.op(Operator::I32Const { value: part_code }, &[], &[Type::I32]);
            return Ok(self.op(
                Operator::Call {
                    function_index: helpers.part,
                },
                &[time, part_val],
                &[Type::F64],
            ));
        }

        if method == "setUTCDate" {
            ensure!(arguments.len() == 1, "Date.setUTCDate accepts 1 argument");
            let time = self.op(
                Operator::F64Load {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        align: 3,
                        offset: 0,
                    },
                },
                &[date],
                &[Type::F64],
            );
            let day_code = self.op(Operator::I32Const { value: 2 }, &[], &[Type::I32]);
            let current_day_f64 = self.op(
                Operator::Call {
                    function_index: helpers.part,
                },
                &[time, day_code],
                &[Type::F64],
            );
            let new_day = self.numeric_operand(&arguments[0])?;
            let new_day = self.op(Operator::F64Trunc, &[new_day], &[Type::F64]);
            let diff_days = self.op(Operator::F64Sub, &[new_day, current_day_f64], &[Type::F64]);
            let ms_per_day = self.op(
                Operator::F64Const {
                    value: 86_400_000_f64.to_bits(),
                },
                &[],
                &[Type::F64],
            );
            let diff_ms = self.op(Operator::F64Mul, &[diff_days, ms_per_day], &[Type::F64]);
            let new_time = self.op(Operator::F64Add, &[time, diff_ms], &[Type::F64]);
            let new_time = self.op(
                Operator::Call {
                    function_index: helpers.clip,
                },
                &[new_time],
                &[Type::F64],
            );
            self.op(
                Operator::F64Store {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        align: 3,
                        offset: 0,
                    },
                },
                &[date, new_time],
                &[],
            );
            return Ok(new_time);
        }

        bail!(
            "Unsupported Date method '{method}'; only getTime(), toISOString(), getUTCDay(), getUTCDate(), getUTCMonth(), getUTCFullYear(), and setUTCDate() are supported"
        )
    }
}
