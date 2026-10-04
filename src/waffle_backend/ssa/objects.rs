//! Evaluate plain object fields in source order and check tags at typed reads.

use super::{FunctionLowerer, options::literal_properties, types::StringKind};
use crate::waffle_backend::{abi, objects::is_object};
use anyhow::{Result, bail, ensure};
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
            return if property.optional {
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
            let (_, tag, payload) = self.object_value(expression)?;
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
        let (original, tag, payload) = self.object_value(expression)?;
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
        let tag = value_tag(value_type)?;
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

    fn object_value(&mut self, expression: &Expr) -> Result<(Value, Value, Value)> {
        let ty = self.infer_expr_type(expression);
        let original = if matches!(expression, Expr::Null) {
            self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])
        } else {
            self.expression(expression)?
        };
        let (tag, value) = if crate::waffle_backend::text_or_bytes::is_text_or_bytes(&ty) {
            let (value, binary) = self.text_or_bytes_parts(original);
            let text = self.op(Operator::I32Const { value: 4 }, &[], &[Type::I32]);
            (
                self.op(Operator::I32Add, &[text, binary], &[Type::I32]),
                value,
            )
        } else if StringKind::of(&ty) == Some(StringKind::Optional) {
            let text = self.op(Operator::I32Const { value: 4 }, &[], &[Type::I32]);
            let undefined = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
            (
                self.op(Operator::Select, &[text, undefined, original], &[Type::I32]),
                original,
            )
        } else {
            let tag = value_tag(&ty)?;
            (
                self.op(Operator::I32Const { value: tag }, &[], &[Type::I32]),
                original,
            )
        };
        let payload = abi::encode_payload(&mut self.body, self.block, Some(value));
        Ok((original, tag, payload))
    }
}

fn value_tag(ty: &HirType) -> Result<u32> {
    Ok(match ty {
        HirType::Void => 0,
        HirType::Null => 1,
        HirType::Boolean => 2,
        HirType::Number => 3,
        HirType::String => 4,
        ty if crate::waffle_backend::bytes::is_byte_view(ty) => 5,
        HirType::Object(_) => 6,
        ty if crate::waffle_backend::filesystem::is_stats(ty) => 7,
        HirType::Array(inner) if **inner == HirType::String => 8,
        ty if crate::waffle_backend::decoder::is_decoder(ty) => 9,
        ty if crate::waffle_backend::date::is_date(ty) => 11,
        HirType::Promise(_) => 10,
        _ => bail!("Unsupported object field type: {ty:?}"),
    })
}
