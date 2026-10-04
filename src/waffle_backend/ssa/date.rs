//! Date coercions and checked receivers using the shared value and completion ABI.

use super::FunctionLowerer;
use crate::waffle_backend::{abi, date::is_date};
use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn new_date(&mut self, arguments: &[Expr]) -> Result<Value> {
        ensure!(
            arguments.len() == 1,
            "Date construction requires one normalized argument"
        );
        let argument = &arguments[0];
        let ty = self.infer_expr_type(argument);
        let time = if is_date(&ty) {
            self.date_method(argument, "getTime", &[])?
        } else if matches!(ty, HirType::Null) {
            self.op(
                Operator::F64Const {
                    value: 0.0f64.to_bits(),
                },
                &[],
                &[Type::F64],
            )
        } else if matches!(ty, HirType::Void) {
            self.expression(argument)?;
            self.op(
                Operator::F64Const {
                    value: f64::NAN.to_bits(),
                },
                &[],
                &[Type::F64],
            )
        } else if ty == HirType::Boolean {
            let value = self.expression(argument)?;
            self.op(Operator::F64ConvertI32U, &[value], &[Type::F64])
        } else {
            ensure!(
                matches!(ty, HirType::Number | HirType::Any),
                "Date construction supports numbers, booleans, null, undefined, and Date copies; string parsing and object coercion are unsupported"
            );
            self.expression(argument)?
        };
        let helper = self
            .registry
            .date_helpers
            .expect("Date helpers are registered")
            .new;
        Ok(self.op(
            Operator::Call {
                function_index: helper,
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
        ensure!(arguments.is_empty(), "Date.{method} accepts no arguments");
        let date = self.expression(receiver)?;
        let helpers = self
            .registry
            .date_helpers
            .expect("Date helpers are registered");
        if method == "getTime" || method == "valueOf" {
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
            let payload = self.call_completion(helpers.iso, &[date]);
            return Ok(abi::decode_payload(
                &mut self.body,
                self.block,
                payload,
                true,
            ));
        }
        let part = match method {
            "getFullYear" | "getUTCFullYear" => 0,
            "getMonth" | "getUTCMonth" => 1,
            "getDate" | "getUTCDate" => 2,
            "getDay" | "getUTCDay" => 3,
            "getHours" | "getUTCHours" => 4,
            "getMinutes" | "getUTCMinutes" => 5,
            "getSeconds" | "getUTCSeconds" => 6,
            "getMilliseconds" | "getUTCMilliseconds" => 7,
            _ => bail!("Unsupported Date method '{method}'"),
        };
        let part = self.op(Operator::I32Const { value: part }, &[], &[Type::I32]);
        Ok(self.op(
            Operator::Call {
                function_index: helpers.part,
            },
            &[date, part],
            &[Type::F64],
        ))
    }
}
