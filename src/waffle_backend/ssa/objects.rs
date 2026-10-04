//! Evaluate plain object fields in source order and check tags at typed reads.

use super::{FunctionLowerer, options::literal_properties, types::StringKind};
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
            .unwrap_or_default();
        HirType::Object(ObjectType {
            properties: fields
                .into_iter()
                .map(|(name, value)| {
                    (
                        name,
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

    pub(super) fn object_property_type(&self, receiver: &Expr, key: &Expr) -> HirType {
        if crate::waffle_backend::context::is_environment(&self.infer_expr_type(receiver)) {
            return HirType::Union(vec![HirType::String, HirType::Void]);
        }
        let receiver_type = self.infer_expr_type(receiver);
        if let HirType::Union(variants) = &receiver_type {
            let Expr::String(key) = key else {
                return HirType::Any;
            };
            let mut types = Vec::new();
            for variant in variants {
                let HirType::Object(record) = variant else {
                    return HirType::Any;
                };
                let Some(field) = record.properties.get(key) else {
                    return HirType::Any;
                };
                if !types.contains(&field.ty) {
                    types.push(field.ty.clone());
                }
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
        if let Expr::String(key) = key
            && let Some(property) = object.properties.get(key)
        {
            return if property.optional && !crate::waffle_backend::values::is_dynamic(&property.ty)
            {
                HirType::Union(vec![property.ty.clone(), HirType::Void])
            } else {
                property.ty.clone()
            };
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
        let object = self.op(
            Operator::Call {
                function_index: helpers.new,
            },
            &[],
            &[Type::I32],
        );
        self.reference_values.insert(object);
        for (name, expression) in fields {
            let field_type = expected
                .and_then(|record| record.properties.get(&name))
                .map(|field| &field.ty);
            let key = self.expression(&Expr::String(name))?;
            let (tag, payload) = if let Some(ty) = field_type {
                let value = self.typed_operand(expression, ty)?;
                self.typed_value_parts(value, ty)?
            } else {
                let (_, tag, payload) = self.tagged_value(expression)?;
                (tag, payload)
            };
            self.call_completion(helpers.set, &[object, key, tag, payload]);
        }
        Ok(object)
    }

    pub(super) fn object_set(
        &mut self,
        receiver: &Expr,
        key: &Expr,
        expression: &Expr,
    ) -> Result<Value> {
        let expected = self.object_property_type(receiver, key);
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
            let value = self.typed_operand(expression, &expected)?;
            let (tag, payload) = self.typed_value_parts(value, &expected)?;
            (value, tag, payload)
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
        let ty = self.object_property_type(receiver, key);
        ensure!(
            ty != HirType::Any,
            "Object property reads need a declared property or index type"
        );
        let optional = StringKind::of(&ty) == Some(StringKind::Optional);
        let value_type = if optional { &HirType::String } else { &ty };
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
        if crate::waffle_backend::values::is_dynamic(&ty)
            || crate::waffle_backend::nullable::inner(&ty).is_some()
        {
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
                value: u32::from(optional),
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
