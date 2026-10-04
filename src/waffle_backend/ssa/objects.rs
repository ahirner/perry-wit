//! Evaluate plain object fields in source order and check tags at typed reads.

use super::{FunctionLowerer, options::literal_properties, types::StringKind};
use crate::waffle_backend::{abi, objects::is_object, values::ValueTag};
use anyhow::{Result, ensure};
use perry_hir::{
    ir::Expr,
    types::{ObjectType, PropertyInfo, Type as HirType},
};
use waffle::{Operator, Type, Value};

impl FunctionLowerer<'_> {
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
        let HirType::Object(object) = self.infer_expr_type(receiver) else {
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

    pub(super) fn new_object(&mut self, expression: &Expr) -> Result<Value> {
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
            let key = self.expression(&Expr::String(name))?;
            let (_, tag, payload) = self.tagged_value(expression)?;
            self.op(
                Operator::Call {
                    function_index: helpers.set,
                },
                &[object, key, tag, payload],
                &[],
            );
        }
        Ok(object)
    }

    pub(super) fn object_set(
        &mut self,
        receiver: &Expr,
        key: &Expr,
        expression: &Expr,
    ) -> Result<Value> {
        let object = self.expression(receiver)?;
        let key = self.string_receiver(key)?;
        let (original, tag, payload) = self.tagged_value(expression)?;
        self.op(
            Operator::Call {
                function_index: self.registry.object_helpers.unwrap().set,
            },
            &[object, key, tag, payload],
            &[],
        );
        Ok(original)
    }

    pub(super) fn object_delete(&mut self, receiver: &Expr, key: &Expr) -> Result<Value> {
        ensure!(
            is_object(&self.infer_expr_type(receiver)),
            "delete requires a plain object receiver"
        );
        let object = self.expression(receiver)?;
        let key = self.string_receiver(key)?;
        Ok(self.op(
            Operator::Call {
                function_index: self.registry.object_helpers.unwrap().delete,
            },
            &[object, key],
            &[Type::I32],
        ))
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
        if crate::waffle_backend::values::is_dynamic(&ty) {
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
}
