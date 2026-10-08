//! Evaluate plain object fields in source order and check tags at typed reads.

use super::{FunctionLowerer, options::literal_properties};
use crate::waffle_backend::{
    abi,
    capabilities::{CapabilityOperation, ContextOperation},
    objects::is_object,
    resolve::TypedIntrinsic,
    values::ValueTag,
};
use anyhow::{Result, ensure};
use perry_hir::{
    ir::Expr,
    types::{ObjectType, PropertyInfo, Type as HirType},
};
use waffle::{Operator, Type, Value};

impl FunctionLowerer<'_> {
    /// Plain own fields retain their last value without exposing the literal's identity.
    pub(super) fn literal_field_projection<'e>(
        &'e self,
        object: &'e Expr,
        property: &str,
    ) -> Option<(usize, &'e Expr)> {
        let mut selected = None;
        for (index, (name, value)) in literal_properties(self.contract, object).ok()??.enumerate() {
            if name == "__proto__" {
                return None;
            }
            if name == property {
                selected = Some((index, value));
            }
        }
        selected
    }

    /// String-only plain fields survive a JSON round trip without conversion or hooks.
    pub(super) fn json_string_projection<'e>(
        &self,
        receiver: &'e Expr,
        property: &str,
    ) -> Option<(&'e Expr, usize)> {
        let serialized = match receiver {
            Expr::JsonParse(input) | Expr::JsonParseTyped { text: input, .. } => input,
            _ => return None,
        };
        let literal = match serialized.as_ref() {
            Expr::JsonStringify(input) => input.as_ref(),
            Expr::JsonStringifyFull(input, replacer, space)
                if matches!(replacer.as_ref(), Expr::Null | Expr::Undefined)
                    && matches!(space.as_ref(), Expr::Null | Expr::Undefined) =>
            {
                input.as_ref()
            }
            _ => return None,
        };
        let mut selected = None;
        for (index, (name, value)) in literal_properties(self.contract, literal)
            .ok()??
            .enumerate()
        {
            if matches!(name, "__proto__" | "toJSON")
                || !crate::waffle_backend::values::is_string_type(&self.infer_expr_type(value))
            {
                return None;
            }
            if name == property {
                selected = Some(index);
            }
        }
        selected.map(|index| (literal, index))
    }

    pub(super) fn project_fields(&mut self, literal: &Expr, selected: usize) -> Result<Value> {
        let fields = literal_properties(self.contract, literal)?.expect("proven literal fields");
        self.project_values(fields.map(|(_, expression)| expression), selected)
    }

    pub(super) fn object_spread(&mut self, parts: Vec<(Option<&str>, &Expr)>) -> Result<Value> {
        let helpers = self.registry.object_helpers.unwrap();
        let capacity = self.op(
            Operator::I32Const {
                value: parts.len().try_into()?,
            },
            &[],
            &[Type::I32],
        );
        let object = self.op(
            Operator::Call {
                function_index: helpers.record,
            },
            &[capacity],
            &[Type::I32],
        );
        self.reference_values.insert(object);
        for (name, expression) in parts {
            let ty = self.infer_expr_type(expression);
            if name.is_none() {
                ensure!(
                    is_object(&ty)
                        || crate::waffle_backend::values::is_dynamic(&ty)
                        || matches!(
                            ty,
                            HirType::Null | HirType::Void | HirType::Number | HirType::Boolean
                        ),
                    "Object spread supports plain objects and non-string primitives"
                );
            }
            let (_, tag, payload) = self.tagged_value(expression)?;
            if let Some(name) = name {
                let key = self.expression(&Expr::String(name.into()))?;
                self.call_completion(helpers.set, &[object, key, tag, payload]);
            } else {
                self.call_completion(helpers.spread, &[object, tag, payload]);
            }
        }
        Ok(object)
    }

    pub(super) fn object_assign(&mut self, target: &Expr, sources: &[Expr]) -> Result<Value> {
        ensure!(
            is_object(&self.infer_expr_type(target)),
            "Object.assign requires a plain object target"
        );
        let target = self.expression(target)?;
        let mut objects = Vec::new();
        for source in sources {
            if matches!(source, Expr::Null | Expr::Undefined) {
                continue;
            }
            let ty = self.infer_expr_type(source);
            ensure!(
                is_object(&ty) || matches!(ty, HirType::Null | HirType::Void),
                "Object.assign sources must be plain objects, null, or undefined"
            );
            let source = self.expression(source)?;
            if is_object(&ty) {
                objects.push(source);
            }
        }
        for source in objects {
            self.call_completion(
                self.registry.object_helpers.unwrap().assign,
                &[target, source],
            );
        }
        Ok(target)
    }

    pub(super) fn object_enumerate(&mut self, receiver: &Expr, values: bool) -> Result<Value> {
        ensure!(
            is_object(&self.infer_expr_type(receiver))
                || crate::waffle_backend::values::is_dynamic(&self.infer_expr_type(receiver)),
            "Object enumeration requires a plain object"
        );
        let object = if crate::waffle_backend::values::is_dynamic(&self.infer_expr_type(receiver)) {
            self.unbox_value(receiver, &HirType::Object(ObjectType::default()))?
        } else {
            self.expression(receiver)?
        };
        let values = self.op(
            Operator::I32Const {
                value: u32::from(values),
            },
            &[],
            &[Type::I32],
        );
        let payload = self.call_completion(
            self.registry.object_helpers.unwrap().enumerate,
            &[object, values],
        );
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            true,
        ))
    }

    pub(super) fn object_has(&mut self, property: &Expr, receiver: &Expr) -> Result<Value> {
        ensure!(
            is_object(&self.infer_expr_type(receiver)),
            "Property membership requires a plain object"
        );
        let key = self.string_receiver(property)?;
        let object = self.expression(receiver)?;
        let entry = self.op(
            Operator::Call {
                function_index: self.registry.object_helpers.unwrap().get,
            },
            &[object, key],
            &[Type::I32],
        );
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        Ok(self.op(Operator::I32Ne, &[entry, zero], &[Type::I32]))
    }

    pub(super) fn object_literal_type(&self, expression: &Expr) -> HirType {
        let fields = literal_properties(self.contract, expression)
            .ok()
            .flatten()
            .into_iter()
            .flatten();
        HirType::Object(ObjectType {
            properties: fields
                .map(|(name, value)| {
                    (
                        name.to_owned(),
                        PropertyInfo {
                            ty: self.infer_expr_type(value),
                            optional: false,
                            readonly: false,
                        },
                    )
                })
                .collect(),
            ..Default::default()
        })
    }

    pub(super) fn object_property_type(&self, receiver: &Expr, key: Option<&str>) -> HirType {
        let receiver_type = self.infer_expr_type(receiver);
        if crate::waffle_backend::context::is_environment(&receiver_type) {
            return HirType::Union(vec![HirType::String, HirType::Void]);
        }
        if let HirType::Union(variants) = &receiver_type {
            let Some(key) = key else {
                return HirType::Any;
            };
            let mut types = Vec::new();
            let mut missing = false;
            for variant in variants {
                let HirType::Object(record) = variant else {
                    return HirType::Any;
                };
                if let Some(field) = record.properties.get(key) {
                    let field_ty = crate::waffle_backend::objects::property_type(field);
                    if !types.contains(&field_ty) {
                        types.push(field_ty);
                    }
                } else {
                    missing = true;
                }
            }
            if missing && !types.contains(&HirType::Void) {
                types.push(HirType::Void);
            }
            if types.is_empty() {
                return HirType::Any;
            }
            return if types.len() == 1 {
                types.remove(0)
            } else {
                HirType::Union(types)
            };
        }
        let HirType::Object(object) = receiver_type else {
            return HirType::Any;
        };
        if let Some(key) = key
            && let Some(property) = object.properties.get(key)
        {
            return crate::waffle_backend::objects::property_type(property);
        }
        object.index_signature.map_or(HirType::Any, |ty| *ty)
    }

    pub(super) fn new_object(
        &mut self,
        expression: &Expr,
        expected: Option<&ObjectType>,
    ) -> Result<Value> {
        let fields = literal_properties(self.contract, expression)?
            .ok_or_else(|| anyhow::anyhow!("Expected a plain object literal"))?;
        let helpers = self.registry.object_helpers.unwrap();
        let count = self.op(
            Operator::I32Const {
                value: fields.clone().count().try_into()?,
            },
            &[],
            &[Type::I32],
        );
        let object = self.op(
            Operator::Call {
                function_index: helpers.record,
            },
            &[count],
            &[Type::I32],
        );
        self.reference_values.insert(object);
        for (name, expression) in fields {
            let field_type = expected
                .and_then(|record| record.properties.get(name))
                .map(crate::waffle_backend::objects::property_type);
            let key = self.expression(&Expr::String(name.into()))?;
            let (tag, payload) = if let Some(ty) = field_type {
                let (_, tag, payload) = self.typed_field_parts(expression, &ty)?;
                (tag, payload)
            } else {
                let (_, tag, payload) = self.tagged_value(expression)?;
                (tag, payload)
            };
            self.call_completion(helpers.set, &[object, key, tag, payload]);
        }
        Ok(object)
    }

    fn typed_field_parts(
        &mut self,
        expression: &Expr,
        expected: &HirType,
    ) -> Result<(Value, Value, Value)> {
        self.check_typed_value(expression, expected)?;
        if matches!(expected, HirType::Union(types) if types.len()==2 && types.contains(&HirType::Number) && types.contains(&HirType::Void))
            && matches!(
                self.infer_expr_type(expression),
                HirType::Number | HirType::Void
            )
        {
            return self.tagged_value(expression);
        }
        let value = self.typed_operand(expression, expected)?;
        let (tag, payload) = self.typed_value_parts(value, expected)?;
        Ok((value, tag, payload))
    }

    pub(super) fn object_set(
        &mut self,
        receiver: &Expr,
        key: &Expr,
        expression: &Expr,
    ) -> Result<Value> {
        let expected = self.object_property_type(
            receiver,
            match key {
                Expr::String(key) => Some(key),
                _ => None,
            },
        );
        if self.contract.wit.is_some() {
            ensure!(
                expected != HirType::Any,
                "Record writes require a declared field"
            );
            self.check_typed_value(expression, &expected)?;
        }
        let object = self.expression(receiver)?;
        let key = self.string_receiver(key)?;
        let (original, tag, payload) = if self.contract.wit.is_some() {
            self.typed_field_parts(expression, &expected)?
        } else {
            self.tagged_value(expression)?
        };
        self.call_completion(
            self.registry.object_helpers.unwrap().set,
            &[object, key, tag, payload],
        );
        Ok(original)
    }

    pub(super) fn object_get(&mut self, receiver: &Expr, key: &Expr) -> Result<Value> {
        let ty = self.object_property_type(
            receiver,
            match key {
                Expr::String(key) => Some(key),
                _ => None,
            },
        );
        ensure!(
            ty != HirType::Any,
            "Object property reads need a declared property or index type"
        );
        let optional = crate::waffle_backend::values::sentinel_inner(&ty);
        let value_type = optional.unwrap_or(&ty);
        let object = self.expression(receiver)?;
        let key = self.string_receiver(key)?;
        let helpers = self.registry.object_helpers.unwrap();
        let entry = self.op(
            Operator::Call {
                function_index: helpers.get,
            },
            &[object, key],
            &[Type::I32],
        );
        if crate::waffle_backend::values::is_boxed(&ty) {
            return Ok(self.op(
                Operator::Call {
                    function_index: helpers.dynamic,
                },
                &[entry],
                &[Type::I32],
            ));
        }
        let tag = ValueTag::of(value_type)? as u32;
        let tag = self.op(Operator::I32Const { value: tag }, &[], &[Type::I32]);
        let optional = self.op(
            Operator::I32Const {
                value: u32::from(optional.is_some()),
            },
            &[],
            &[Type::I32],
        );
        let payload = self.call_completion(helpers.value, &[entry, tag, optional]);
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            value_type != &HirType::Number,
        ))
    }

    /// Perry duplicates the assignment receiver; only proven identical references may be skipped.
    pub(super) fn same_object_reference(&self, target: &Expr, receiver: &Expr) -> bool {
        match (target, receiver) {
            (Expr::LocalGet(left), Expr::LocalGet(right)) => left == right,
            (
                Expr::PropertyGet {
                    object: left,
                    property: left_key,
                    ..
                },
                Expr::PropertyGet {
                    object: right,
                    property: right_key,
                    ..
                },
            ) => left_key == right_key && self.same_object_reference(left, right),
            (
                Expr::Call {
                    callee: left,
                    args: left_args,
                    ..
                },
                Expr::Call {
                    callee: right,
                    args: right_args,
                    ..
                },
            ) if left_args.is_empty() && right_args.is_empty() => {
                matches!((left.as_ref(), right.as_ref()),
                    (Expr::ExternFuncRef { name: left, .. }, Expr::ExternFuncRef { name: right, .. })
                    if left == right && matches!(self.contract.intrinsics.get(left),
                        Some(TypedIntrinsic::Capability(CapabilityOperation::Context(ContextOperation::Environment)))))
            }
            _ => false,
        }
    }
}
