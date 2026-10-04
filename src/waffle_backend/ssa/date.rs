//! Numeric Date construction and immutable UTC operations.

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
        ensure!(
            ty == HirType::Number,
            "Date construction requires a statically known number of epoch milliseconds; coercion and parsing are unsupported"
        );
        let time = self.expression(argument)?;
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
        if method == "getTime" {
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
        bail!("Unsupported Date method '{method}'; only getTime() and toISOString() are supported")
    }
}
