//! Boxing and checked extraction at heterogeneous guest boundaries.

use super::{
    FunctionLowerer,
    types::{StringKind, is_reference},
};
use crate::waffle_backend::{
    abi,
    text_or_bytes::is_text_or_bytes,
    values::{ValueTag, is_boxed, is_dynamic},
};
use anyhow::{Result, ensure};
use perry_hir::{
    ir::{CompareOp, Expr},
    types::Type as HirType,
};
use waffle::{MemoryArg, Operator, Type, Value};

pub(super) enum ArrayElementTypes<'a> {
    Inferred,
    Uniform(&'a HirType),
    Tuple(&'a [HirType]),
}

impl FunctionLowerer<'_> {
    pub(super) fn dynamic_has(&mut self, receiver: Value, key: Value) -> Result<Value> {
        let result =
            self.call_completion(self.registry.value_access.unwrap().has, &[receiver, key]);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            result,
            true,
        ))
    }

    pub(super) fn dynamic_get(&mut self, receiver: &Expr, key: &Expr) -> Result<Value> {
        let receiver = self.value_operand(receiver)?;
        let key = self.value_operand(key)?;
        let payload = self.call_completion(
            self.registry
                .value_access
                .expect("dynamic property helpers")
                .get,
            &[receiver, key],
        );
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    pub(super) fn dynamic_set(
        &mut self,
        receiver: &Expr,
        key: &Expr,
        expression: &Expr,
    ) -> Result<Value> {
        let receiver = self.value_operand(receiver)?;
        let key = self.value_operand(key)?;
        let (original, tag, payload) = self.tagged_value(expression)?;
        let stored = if is_dynamic(&self.infer_expr_type(expression))
            || crate::waffle_backend::nullable::inner(&self.infer_expr_type(expression)).is_some()
        {
            original
        } else {
            self.box_value(tag, payload)
        };
        self.call_completion(
            self.registry
                .value_access
                .expect("dynamic property helpers")
                .set,
            &[receiver, key, stored],
        );
        Ok(original)
    }

    pub(super) fn new_value_array(
        &mut self,
        items: &[Expr],
        types: ArrayElementTypes<'_>,
    ) -> Result<Value> {
        let length = self.op(
            Operator::I32Const {
                value: items.len().try_into()?,
            },
            &[],
            &[Type::I32],
        );
        let array = self.op(
            Operator::Call {
                function_index: self.registry.value_access.unwrap().array_new,
            },
            &[length],
            &[Type::I32],
        );
        self.reference_values.insert(array);
        for (index, item) in items.iter().enumerate() {
            let expected = match types {
                ArrayElementTypes::Inferred => None,
                ArrayElementTypes::Uniform(ty) => Some(ty),
                ArrayElementTypes::Tuple(types) => Some(&types[index]),
            };
            let value = if let Some(ty) = expected {
                let value = self.typed_operand(item, ty)?;
                self.box_typed_value(value, ty)?
            } else {
                self.value_operand(item)?
            };
            self.op(
                Operator::I32Store {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        align: 2,
                        offset: (8 + index * 4).try_into()?,
                    },
                },
                &[array, value],
                &[],
            );
        }
        Ok(array)
    }

    pub(super) fn value_operand(&mut self, expression: &Expr) -> Result<Value> {
        let ty = self.infer_expr_type(expression);
        if is_boxed(&ty) {
            return self.expression(expression);
        }
        let (_, tag, payload) = self.tagged_value(expression)?;
        Ok(self.box_value(tag, payload))
    }

    pub(super) fn box_value(&mut self, tag: Value, payload: Value) -> Value {
        let value = self.op(
            Operator::Call {
                function_index: self
                    .registry
                    .value_helpers
                    .expect("value helpers are registered")
                    .new,
            },
            &[tag, payload],
            &[Type::I32],
        );
        self.reference_values.insert(value);
        value
    }

    pub(super) fn value_parts(&mut self, value: Value) -> (Value, Value) {
        let tag = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    memory: self.registry.memory,
                    align: 2,
                    offset: 0,
                },
            },
            &[value],
            &[Type::I32],
        );
        let payload = self.op(
            Operator::F64Load {
                memory: MemoryArg {
                    memory: self.registry.memory,
                    align: 3,
                    offset: 8,
                },
            },
            &[value],
            &[Type::F64],
        );
        (tag, payload)
    }

    pub(super) fn unbox_value(&mut self, expression: &Expr, expected: &HirType) -> Result<Value> {
        let value = self.expression(expression)?;
        self.extract_value(value, expected)
    }

    pub(super) fn extract_value(&mut self, value: Value, expected: &HirType) -> Result<Value> {
        if is_boxed(expected) {
            return Ok(value);
        }
        let optional = crate::waffle_backend::values::sentinel_inner(expected);
        let expected = optional.unwrap_or(expected);
        let tag = ValueTag::of(expected)? as u32;
        let tag = self.op(Operator::I32Const { value: tag }, &[], &[Type::I32]);
        let helpers = self
            .registry
            .value_helpers
            .expect("value helpers are registered");
        let payload = self.call_completion(
            if optional.is_some() {
                helpers.extract_optional
            } else {
                helpers.extract
            },
            &[value, tag],
        );
        let value = abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            expected != &HirType::Number,
        );
        if is_reference(expected) {
            self.reference_values.insert(value);
        }
        Ok(value)
    }

    pub(super) fn tagged_value(&mut self, expression: &Expr) -> Result<(Value, Value, Value)> {
        let ty = self.infer_expr_type(expression);
        let original = if matches!(expression, Expr::Null) {
            self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])
        } else {
            self.expression(expression)?
        };
        let (tag, payload) = self.typed_value_parts(original, &ty)?;
        Ok((original, tag, payload))
    }

    pub(super) fn box_typed_value(&mut self, value: Value, ty: &HirType) -> Result<Value> {
        if is_boxed(ty) {
            return Ok(value);
        }
        let (tag, payload) = self.typed_value_parts(value, ty)?;
        Ok(self.box_value(tag, payload))
    }

    pub(super) fn typed_value_parts(
        &mut self,
        original: Value,
        ty: &HirType,
    ) -> Result<(Value, Value)> {
        if is_boxed(ty) {
            return Ok(self.value_parts(original));
        }
        let (tag, value) = if is_text_or_bytes(ty) {
            let (value, binary) = self.text_or_bytes_parts(original);
            let text = self.op(
                Operator::I32Const {
                    value: ValueTag::String as u32,
                },
                &[],
                &[Type::I32],
            );
            (
                self.op(Operator::I32Add, &[text, binary], &[Type::I32]),
                value,
            )
        } else if StringKind::of(ty) == Some(StringKind::Optional) {
            let text = self.op(
                Operator::I32Const {
                    value: ValueTag::String as u32,
                },
                &[],
                &[Type::I32],
            );
            let undefined = self.op(
                Operator::I32Const {
                    value: ValueTag::Undefined as u32,
                },
                &[],
                &[Type::I32],
            );
            (
                self.op(Operator::Select, &[text, undefined, original], &[Type::I32]),
                original,
            )
        } else if matches!(ty, HirType::Union(types) if types.len() == 2 && types.contains(&HirType::Number) && types.contains(&HirType::Void))
        {
            let defined = self.op(Operator::F64Eq, &[original, original], &[Type::I32]);
            let number = self.op(
                Operator::I32Const {
                    value: ValueTag::Number as u32,
                },
                &[],
                &[Type::I32],
            );
            let undefined = self.op(
                Operator::I32Const {
                    value: ValueTag::Undefined as u32,
                },
                &[],
                &[Type::I32],
            );
            let empty = self.op(
                Operator::F64Const {
                    value: 0.0f64.to_bits(),
                },
                &[],
                &[Type::F64],
            );
            (
                self.op(
                    Operator::Select,
                    &[number, undefined, defined],
                    &[Type::I32],
                ),
                self.op(Operator::Select, &[original, empty, defined], &[Type::F64]),
            )
        } else {
            let tag = ValueTag::of(ty)? as u32;
            (
                self.op(Operator::I32Const { value: tag }, &[], &[Type::I32]),
                original,
            )
        };
        let payload = abi::encode_payload(&mut self.body, self.block, Some(value));
        Ok((tag, payload))
    }

    pub(super) fn value_comparison(
        &mut self,
        operation: CompareOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<Value> {
        ensure!(
            matches!(operation, CompareOp::Eq | CompareOp::Ne),
            "Dynamic values support strict equality; coercive comparisons are unsupported"
        );
        let left = self.value_operand(left)?;
        let right = self.value_operand(right)?;
        let equal = self.op(
            Operator::Call {
                function_index: self
                    .registry
                    .value_helpers
                    .expect("value helpers are registered")
                    .equal,
            },
            &[left, right],
            &[Type::I32],
        );
        Ok(if operation == CompareOp::Ne {
            self.op(Operator::I32Eqz, &[equal], &[Type::I32])
        } else {
            equal
        })
    }

    pub(super) fn value_typeof(&mut self, value: Value) -> Result<Value> {
        let (tag, _) = self.value_parts(value);
        let mut result = self.expression(&Expr::String("object".into()))?;
        for (kind, label) in [
            (ValueTag::Undefined, "undefined"),
            (ValueTag::Boolean, "boolean"),
            (ValueTag::Number, "number"),
            (ValueTag::String, "string"),
        ] {
            let expected = self.op(Operator::I32Const { value: kind as u32 }, &[], &[Type::I32]);
            let matches = self.op(Operator::I32Eq, &[tag, expected], &[Type::I32]);
            let label = self.expression(&Expr::String(label.into()))?;
            result = self.op(Operator::Select, &[label, result, matches], &[Type::I32]);
        }
        Ok(result)
    }

    pub(super) fn numeric_operand(&mut self, expression: &Expr) -> Result<Value> {
        let value = self.expression(expression)?;
        if is_dynamic(&self.infer_expr_type(expression)) {
            Ok(self.call_completion(
                self.registry
                    .value_helpers
                    .expect("value helpers are registered")
                    .scalar_number,
                &[value],
            ))
        } else {
            Ok(value)
        }
    }
}
