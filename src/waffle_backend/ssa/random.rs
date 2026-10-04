//! Validate supported random destinations after preserving their evaluation effects.

use super::FunctionLowerer;
use crate::waffle_backend::{abi, bytes::is_byte_view, text_or_bytes::is_text_or_bytes};
use anyhow::{Result, ensure};
use perry_hir::ir::Expr;
use waffle::{Operator, Type, Value};

impl FunctionLowerer<'_> {
    fn discard_random_argument(&mut self, expression: &Expr) -> Result<()> {
        match expression {
            Expr::Null => {}
            Expr::Array(elements) => {
                for element in elements {
                    self.discard_random_argument(element)?;
                }
            }
            _ => {
                self.expression(expression)?;
            }
        }
        Ok(())
    }

    pub(super) fn random_fill(&mut self, name: &str, arguments: &[Expr]) -> Result<Value> {
        ensure!(
            arguments.len() == 1,
            "crypto.getRandomValues expects one argument"
        );
        let argument = &arguments[0];
        let ty = self.infer_expr_type(argument);
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let (view, valid) = if is_byte_view(&ty) {
            let view = self.expression(argument)?;
            let valid = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
            (view, valid)
        } else if is_text_or_bytes(&ty) {
            let value = self.expression(argument)?;
            self.text_or_bytes_parts(value)
        } else {
            self.discard_random_argument(argument)?;
            (zero, zero)
        };
        let result = self.call_completion(self.registry.intrinsics[name], &[view, valid]);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            result,
            true,
        ))
    }
}
