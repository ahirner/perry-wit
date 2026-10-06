//! Source byte construction, indexed mutation, and alias-preserving views.

use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

use super::FunctionLowerer;
use crate::waffle_backend::{abi, bytes::is_byte_view};

/// The constructor cannot escape an immediate, known in-bounds indexed read.
pub(super) fn literal_projection<'a>(array: &'a Expr, index: &Expr) -> Option<(&'a [Expr], usize)> {
    let Expr::Uint8ArrayNew(Some(argument)) = array else {
        return None;
    };
    super::arrays::literal_projection(argument, index)
}

impl FunctionLowerer<'_> {
    pub(super) fn encode_bytes(&mut self, input: &Expr) -> Result<Value> {
        ensure!(
            self.is_string(input),
            "TextEncoder.encode requires a string"
        );
        let string = self.expression(input)?;
        let mut parts = Vec::new();
        for offset in [0, 4] {
            parts.push(self.op(
                Operator::I32Load {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        offset,
                        align: 2,
                    },
                },
                &[string],
                &[Type::I32],
            ));
        }
        let helpers = self.registry.byte_helpers.unwrap();
        let view = self.op(
            Operator::Call {
                function_index: helpers.lift_canonical,
            },
            &parts,
            &[Type::I32],
        );
        Ok(self.op(
            Operator::Call {
                function_index: helpers.copy,
            },
            &[view],
            &[Type::I32],
        ))
    }

    pub(super) fn byte_receiver(&mut self, expression: &Expr) -> Result<Value> {
        ensure!(
            is_byte_view(&self.infer_expr_type(expression)),
            "Expected a Uint8Array receiver"
        );
        self.expression(expression)
    }

    pub(super) fn new_bytes(&mut self, argument: Option<&Expr>) -> Result<Value> {
        let helpers = self
            .registry
            .byte_helpers
            .expect("byte helpers are registered");
        if let Some(argument) = argument.filter(|argument| {
            crate::waffle_backend::bytes::is_array_buffer(&self.infer_expr_type(argument))
        }) {
            let buffer = self.expression(argument)?;
            let mut parts = Vec::new();
            for offset in [0, 4] {
                parts.push(self.op(
                    Operator::I32Load {
                        memory: MemoryArg {
                            align: 2,
                            offset,
                            memory: self.registry.memory,
                        },
                    },
                    &[buffer],
                    &[Type::I32],
                ));
            }
            return Ok(self.op(
                Operator::Call {
                    function_index: helpers.lift_canonical,
                },
                &parts,
                &[Type::I32],
            ));
        }
        if let Some(argument) =
            argument.filter(|argument| is_byte_view(&self.infer_expr_type(argument)))
        {
            let source = self.byte_receiver(argument)?;
            return Ok(self.op(
                Operator::Call {
                    function_index: helpers.copy,
                },
                &[source],
                &[Type::I32],
            ));
        }
        let (length, elements) = if let Some(Expr::Array(elements)) = argument {
            let elements = elements
                .iter()
                .map(|element| self.byte_number(Some(element), f64::NAN))
                .collect::<Result<Vec<_>>>()?;
            let length = self.op(
                Operator::F64Const {
                    value: (elements.len() as f64).to_bits(),
                },
                &[],
                &[Type::F64],
            );
            (length, elements)
        } else {
            (self.byte_number(argument, 0.0)?, Vec::new())
        };
        let payload = self.call_completion(helpers.new, &[length]);
        let view = abi::decode_payload(&mut self.body, self.block, payload, true);
        for (index, value) in elements.into_iter().enumerate() {
            let index = self.op(
                Operator::F64Const {
                    value: (index as f64).to_bits(),
                },
                &[],
                &[Type::F64],
            );
            self.op(
                Operator::Call {
                    function_index: helpers.set,
                },
                &[view, index, value],
                &[],
            );
        }
        Ok(view)
    }

    pub(super) fn byte_index(&mut self, array: &Expr, index: &Expr) -> Result<Value> {
        if let Some((items, selected)) = literal_projection(array, index) {
            let mut result = None;
            for (index, item) in items.iter().enumerate() {
                let value = self.byte_number(Some(item), f64::NAN)?;
                if index == selected {
                    result = Some(value);
                }
            }
            let byte = self.op(
                Operator::Call {
                    function_index: self.registry.byte_helpers.unwrap().to_byte,
                },
                &[result.expect("literal projection validated the index")],
                &[Type::I32],
            );
            return Ok(self.op(Operator::F64ConvertI32U, &[byte], &[Type::F64]));
        }
        let view = self.byte_receiver(array)?;
        let index = self.byte_number(Some(index), f64::NAN)?;
        let get = self.registry.byte_helpers.unwrap().get;
        Ok(self.op(
            Operator::Call {
                function_index: get,
            },
            &[view, index],
            &[Type::F64],
        ))
    }

    pub(super) fn byte_set(&mut self, array: &Expr, index: &Expr, value: &Expr) -> Result<Value> {
        let view = self.byte_receiver(array)?;
        let index = self.byte_number(Some(index), f64::NAN)?;
        let original = self.expression(value)?;
        let number = self.coerce_byte_number(original, &self.infer_expr_type(value))?;
        let set = self.registry.byte_helpers.unwrap().set;
        self.op(
            Operator::Call {
                function_index: set,
            },
            &[view, index, number],
            &[],
        );
        Ok(original)
    }

    pub(super) fn byte_property(&mut self, array: &Expr, property: &str) -> Result<Value> {
        let ty = self.infer_expr_type(array);
        ensure!(
            crate::waffle_backend::bytes::is_byte_storage(&ty),
            "Expected byte storage"
        );
        ensure!(
            !crate::waffle_backend::bytes::is_array_buffer(&ty) || property == "byteLength",
            "Unsupported ArrayBuffer property '{property}'"
        );
        let view = self.expression(array)?;
        let offset = match property {
            "length" | "byteLength" => 4,
            "byteOffset" => 12,
            _ => bail!("Unsupported Uint8Array property '{property}'"),
        };
        let value = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset,
                    memory: self.registry.memory,
                },
            },
            &[view],
            &[Type::I32],
        );
        Ok(self.op(Operator::F64ConvertI32U, &[value], &[Type::F64]))
    }

    pub(super) fn byte_method(
        &mut self,
        array: &Expr,
        method: &str,
        arguments: &[Expr],
    ) -> Result<Value> {
        if method == "set" {
            ensure!(
                !arguments.is_empty() && arguments.len() <= 2,
                "Uint8Array.set requires a Uint8Array and optional numeric offset"
            );
            let view = self.byte_receiver(array)?;
            let source = self.byte_receiver(&arguments[0])?;
            let offset = self.byte_number(arguments.get(1), 0.0)?;
            return Ok(self.call_completion(
                self.registry.byte_helpers.unwrap().copy_into,
                &[view, source, offset],
            ));
        }
        ensure!(
            matches!(method, "subarray" | "slice"),
            "Unsupported Uint8Array method '{method}'"
        );
        ensure!(
            arguments.len() <= 2,
            "Uint8Array {method} supports at most two bounds"
        );
        let view = self.byte_receiver(array)?;
        let start = self.byte_number(arguments.first(), 0.0)?;
        let end = self.byte_number(arguments.get(1), f64::INFINITY)?;
        let helpers = self.registry.byte_helpers.unwrap();
        let result = self.op(
            Operator::Call {
                function_index: helpers.subarray,
            },
            &[view, start, end],
            &[Type::I32],
        );
        Ok(if method == "slice" {
            self.op(
                Operator::Call {
                    function_index: helpers.copy,
                },
                &[result],
                &[Type::I32],
            )
        } else {
            result
        })
    }

    fn byte_number(&mut self, expression: Option<&Expr>, default: f64) -> Result<Value> {
        let constant = match expression {
            None | Some(Expr::Undefined) => Some(default),
            Some(Expr::Null) => Some(0.0),
            _ => None,
        };
        if let Some(number) = constant {
            return Ok(self.op(
                Operator::F64Const {
                    value: number.to_bits(),
                },
                &[],
                &[Type::F64],
            ));
        }
        let expression = expression.unwrap();
        let value = self.expression(expression)?;
        if self.infer_expr_type(expression) == HirType::Void {
            return Ok(self.op(
                Operator::F64Const {
                    value: default.to_bits(),
                },
                &[],
                &[Type::F64],
            ));
        }
        self.coerce_byte_number(value, &self.infer_expr_type(expression))
    }

    fn coerce_byte_number(&mut self, value: Value, ty: &HirType) -> Result<Value> {
        match ty {
            HirType::Number => Ok(value),
            HirType::Boolean => Ok(self.op(Operator::F64ConvertI32U, &[value], &[Type::F64])),
            HirType::Void => Ok(self.op(
                Operator::F64Const {
                    value: f64::NAN.to_bits(),
                },
                &[],
                &[Type::F64],
            )),
            HirType::Union(types) if types == &[HirType::Number, HirType::Void] => Ok(value),
            _ => bail!("Uint8Array numeric arguments do not support coercion from {ty:?}"),
        }
    }
}
