use super::FunctionLowerer;
use crate::waffle_backend::{abi, errors};
use anyhow::{Result, ensure};
use perry_hir::ir::Expr;
use waffle::{Operator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn normalize_error(&mut self, status: Value, payload: Value) -> Option<Value> {
        if let Some(values) = self.registry.value_helpers {
            Some(
                self.op(
                    Operator::Call {
                        function_index: self
                            .registry
                            .errors
                            .map_or(values.exception, |errors| errors.normalize),
                    },
                    &[status, payload],
                    &[Type::F64],
                ),
            )
        } else {
            abi::emit_completion(
                &mut self.body,
                self.block,
                abi::CompletionStatus::NativeFailure,
                payload,
            );
            None
        }
    }

    pub(super) fn emit_native_throw(&mut self, payload: Value) {
        let status = self.op(
            Operator::I32Const {
                value: abi::CompletionStatus::NativeFailure as u32,
            },
            &[],
            &[Type::I32],
        );
        if let Some(payload) = self.normalize_error(status, payload) {
            self.emit_throw(payload);
        }
    }

    pub(super) fn error_constructor(&mut self, expression: &Expr) -> Result<Value> {
        let (kind, message, cause, options) = match expression {
            Expr::ErrorNew(message) => (0, message.as_deref(), None, None),
            Expr::ErrorNewWithCause { message, cause } => {
                (0, Some(message.as_ref()), Some(cause.as_ref()), None)
            }
            Expr::ErrorNewWithOptions {
                kind,
                message,
                options,
            } => (*kind, Some(message.as_ref()), None, Some(options.as_ref())),
            Expr::TypeErrorNew(message) => (1, Some(message.as_ref()), None, None),
            Expr::RangeErrorNew(message) => (2, Some(message.as_ref()), None, None),
            Expr::ReferenceErrorNew(message) => (3, Some(message.as_ref()), None, None),
            Expr::SyntaxErrorNew(message) => (4, Some(message.as_ref()), None, None),
            _ => unreachable!(),
        };
        let message = match message {
            None | Some(Expr::Undefined) => self.expression(&Expr::String(String::new()))?,
            Some(message) => self.string_receiver(message)?,
        };
        let cause = if let Some(cause) = cause {
            self.value_operand(cause)?
        } else if let Some(options) = options {
            let options = self.value_operand(options)?;
            self.op(
                Operator::Call {
                    function_index: self.registry.errors.unwrap().cause,
                },
                &[options],
                &[Type::I32],
            )
        } else {
            self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])
        };
        let kind = self.op(Operator::I32Const { value: kind }, &[], &[Type::I32]);
        let value = self.op(
            Operator::Call {
                function_index: self.registry.errors.unwrap().new,
            },
            &[kind, message, cause],
            &[Type::I32],
        );
        self.reference_values.insert(value);
        Ok(value)
    }

    pub(super) fn error_instanceof(
        &mut self,
        expression: &Expr,
        name: &str,
        dynamic: &Option<Box<Expr>>,
    ) -> Result<Value> {
        ensure!(
            dynamic.is_none(),
            "Dynamic instanceof constructors are unsupported"
        );
        let kind = errors::NAMES
            .iter()
            .position(|candidate| *candidate == name)
            .ok_or_else(|| anyhow::anyhow!("instanceof {name} is unsupported"))?;
        let value = self.value_operand(expression)?;
        let kind = self.op(Operator::I32Const { value: kind as u32 }, &[], &[Type::I32]);
        Ok(self.op(
            Operator::Call {
                function_index: self.registry.errors.unwrap().is,
            },
            &[value, kind],
            &[Type::I32],
        ))
    }

    pub(super) fn error_message(&mut self, expression: &Expr) -> Result<Value> {
        let value = self.dynamic_get(expression, &Expr::String("message".into()))?;
        self.extract_value(value, &perry_hir::types::Type::String)
    }
}
