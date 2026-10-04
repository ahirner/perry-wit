//! Decoder receiver checks and source-order evaluation of supported options.

use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

use super::FunctionLowerer;
use crate::waffle_backend::{abi, decoder::is_decoder};

impl FunctionLowerer<'_> {
    pub(super) fn decoder_receiver(&mut self, expression: &Expr) -> Result<Value> {
        ensure!(
            is_decoder(&self.infer_expr_type(expression)),
            "Expected a TextDecoder receiver"
        );
        self.expression(expression)
    }

    pub(super) fn new_decoder(&mut self, arguments: &[Expr]) -> Result<Value> {
        ensure!(
            arguments.len() <= 2,
            "TextDecoder construction accepts a label and options"
        );
        let label = match arguments.first() {
            Some(expression) if self.infer_expr_type(expression) != HirType::Void => {
                self.string_receiver(expression)?
            }
            expression => {
                if let Some(expression) = expression {
                    self.expression(expression)?;
                }
                self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])
            }
        };
        let options =
            self.decoder_options(arguments.get(1), &[("fatal", true), ("ignoreBOM", false)])?;
        let function = self
            .registry
            .decoder_helpers
            .expect("decoder helpers are registered")
            .new;
        let payload = self.call_completion(function, &[label, options[0], options[1]]);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    pub(super) fn decode_bytes(&mut self, receiver: &Expr, arguments: &[Expr]) -> Result<Value> {
        ensure!(
            arguments.len() <= 2,
            "TextDecoder.decode accepts bytes and options"
        );
        let decoder = self.decoder_receiver(receiver)?;
        let bytes = match arguments.first() {
            Some(expression) if self.infer_expr_type(expression) != HirType::Void => {
                self.byte_receiver(expression)?
            }
            expression => {
                if let Some(expression) = expression {
                    self.expression(expression)?;
                }
                self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])
            }
        };
        let options = self.decoder_options(arguments.get(1), &[("stream", false)])?;
        let function = self
            .registry
            .decoder_helpers
            .expect("decoder helpers are registered")
            .decode;
        let payload = self.call_completion(function, &[decoder, bytes, options[0]]);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    pub(super) fn decoder_property(&mut self, receiver: &Expr, property: &str) -> Result<Value> {
        let decoder = self.decoder_receiver(receiver)?;
        match property {
            "encoding" => Ok(self.op(
                Operator::I32Const {
                    value: self
                        .string_pool
                        .get("utf-8")
                        .expect("decoder encoding is interned"),
                },
                &[],
                &[Type::I32],
            )),
            "fatal" => Ok(self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32])),
            "ignoreBOM" => Ok(self.op(
                Operator::I32Load {
                    memory: MemoryArg {
                        align: 2,
                        offset: 0,
                        memory: self.registry.memory,
                    },
                },
                &[decoder],
                &[Type::I32],
            )),
            _ => bail!("Unsupported TextDecoder property '{property}'"),
        }
    }

    fn decoder_options(
        &mut self,
        expression: Option<&Expr>,
        defaults: &[(&str, bool)],
    ) -> Result<Vec<Value>> {
        let mut values = defaults
            .iter()
            .map(|(_, value)| {
                self.op(
                    Operator::I32Const {
                        value: u32::from(*value),
                    },
                    &[],
                    &[Type::I32],
                )
            })
            .collect::<Vec<_>>();
        let Some(expression) = expression else {
            return Ok(values);
        };
        if matches!(expression, Expr::Null | Expr::Undefined) {
            return Ok(values);
        }
        if self.infer_expr_type(expression) == HirType::Void {
            self.expression(expression)?;
            return Ok(values);
        }
        let properties: Vec<_> = match expression {
            Expr::Object(properties) => properties
                .iter()
                .map(|(key, value)| (key.as_str(), value))
                .collect(),
            Expr::New {
                class_name, args, ..
            } if self.contract.literal_shapes.contains_key(class_name) => {
                let fields = &self.contract.literal_shapes[class_name];
                ensure!(
                    fields.len() == args.len(),
                    "Literal object shape does not match its values"
                );
                fields.iter().map(String::as_str).zip(args).collect()
            }
            _ => bail!("Decoder options require a literal object, null, or undefined"),
        };
        for (name, expression) in properties {
            if let Some(index) = defaults.iter().position(|(key, _)| name == *key) {
                values[index] = if self.infer_expr_type(expression) == HirType::Void {
                    self.expression(expression)?;
                    self.op(
                        Operator::I32Const {
                            value: u32::from(defaults[index].1),
                        },
                        &[],
                        &[Type::I32],
                    )
                } else if matches!(expression, Expr::Null) {
                    self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])
                } else {
                    self.condition(expression)?
                };
            } else if !matches!(expression, Expr::Null) {
                self.expression(expression)?;
            }
        }
        Ok(values)
    }
}
