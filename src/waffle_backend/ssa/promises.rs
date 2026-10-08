//! Statically typed Promise combinators; no thenable or iterator reflection.

use super::{FunctionLowerer, values::ArrayElementTypes};
use crate::waffle_backend::{
    capabilities::CapabilityOperation,
    promises::{Combinator, OPTIONAL_REFERENCE_TAG},
    resolve::TypedIntrinsic,
    values::ValueTag,
};
use anyhow::{Result, bail, ensure};
use perry_hir::{
    ir::Expr,
    types::{ObjectType, PropertyInfo, Type as HirType},
};
use std::collections::HashMap;
use waffle::{Operator, Type, Value};

fn settlement(ty: HirType) -> HirType {
    let record = |status: &str, field: &str, ty| {
        HirType::Object(ObjectType {
            name: None,
            properties: HashMap::from([
                (
                    "status".into(),
                    PropertyInfo {
                        ty: HirType::StringLiteral(status.into()),
                        optional: false,
                        readonly: false,
                    },
                ),
                (
                    field.into(),
                    PropertyInfo {
                        ty,
                        optional: false,
                        readonly: false,
                    },
                ),
            ]),
            ..Default::default()
        })
    };
    HirType::Union(vec![
        record("fulfilled", "value", ty),
        record("rejected", "reason", HirType::Number),
    ])
}
impl FunctionLowerer<'_> {
    pub(super) fn combinator(&self, callee: &Expr) -> Option<Combinator> {
        let Expr::ExternFuncRef { name, .. } = callee else {
            return None;
        };
        match self.contract.intrinsics.get(name) {
            Some(TypedIntrinsic::Capability(CapabilityOperation::Promise(operation))) => {
                Some(*operation)
            }
            _ => None,
        }
    }
    fn combinator_input(&self, arg: &Expr) -> Result<HirType> {
        if let Expr::Array(items) = arg {
            return Ok(HirType::Tuple(
                items
                    .iter()
                    .map(|item| self.infer_expr_type(item))
                    .collect(),
            ));
        }
        match self.infer_expr_type(arg) {
            ty @ (HirType::Array(_) | HirType::Tuple(_)) => Ok(ty),
            _ => bail!("Promise combinators require a dense typed array or tuple"),
        }
    }
    pub(super) fn combinator_type(&self, operation: Combinator, args: &[Expr]) -> Result<HirType> {
        ensure!(
            args.len() == 1,
            "Promise.{} expects one dense array or tuple",
            operation.name()
        );
        let input = self.combinator_input(&args[0])?;
        let element = |ty| {
            let ty = match ty {
                HirType::Promise(inner) => *inner,
                ty => ty,
            };
            if operation == Combinator::AllSettled {
                settlement(ty)
            } else {
                ty
            }
        };
        let result = match input {
            HirType::Array(inner) if operation == Combinator::Race => element(*inner),
            HirType::Array(inner) => HirType::Array(Box::new(element(*inner))),
            HirType::Tuple(types) if operation == Combinator::Race => {
                let mut outcomes = Vec::new();
                for ty in types.into_iter().map(element) {
                    if let HirType::Union(variants) = ty {
                        for ty in variants {
                            if !outcomes.contains(&ty) {
                                outcomes.push(ty);
                            }
                        }
                    } else if !outcomes.contains(&ty) {
                        outcomes.push(ty);
                    }
                }
                match outcomes.len() {
                    0 => HirType::Void,
                    1 => outcomes.remove(0),
                    _ => HirType::Union(outcomes),
                }
            }
            HirType::Tuple(types) => HirType::Tuple(types.into_iter().map(element).collect()),
            _ => unreachable!(),
        };
        Ok(HirType::Promise(Box::new(result)))
    }
    pub(super) fn combine_promises(
        &mut self,
        operation: Combinator,
        args: &[Expr],
    ) -> Result<Value> {
        let ty = self.combinator_type(operation, args)?;
        let input_ty = self.combinator_input(&args[0])?;
        let HirType::Promise(result) = ty else {
            unreachable!()
        };
        let outcome_tag = |ty: &HirType| -> Result<u32> {
            let ty = match ty {
                HirType::Promise(inner) => inner.as_ref(),
                ty => ty,
            };
            if crate::waffle_backend::values::is_boxed(ty) {
                Ok(255)
            } else if let Some(inner) = crate::waffle_backend::values::sentinel_inner(ty)
                && inner != &HirType::Number
            {
                Ok(OPTIONAL_REFERENCE_TAG | ValueTag::of(inner)? as u32)
            } else {
                Ok(ValueTag::of(ty)? as u32)
            }
        };
        let tags = match &input_ty {
            HirType::Array(inner) => {
                let tag = outcome_tag(inner)?;
                self.op(Operator::I32Const { value: tag }, &[], &[Type::I32])
            }
            HirType::Tuple(types) => {
                let tags = types
                    .iter()
                    .map(|ty| Ok(Expr::Number(outcome_tag(ty)? as f64)))
                    .collect::<Result<Vec<_>>>()?;
                self.new_value_array(&tags, ArrayElementTypes::Uniform(&HirType::Number))?
            }
            _ => unreachable!(),
        };
        let input = if let Expr::Array(items) = &args[0] {
            let HirType::Tuple(types) = &input_ty else {
                unreachable!()
            };
            self.new_value_array(items, ArrayElementTypes::Tuple(types))?
        } else {
            self.expression(&args[0])?
        };
        let inputs = [
            operation as u32,
            if super::types::is_reference(&result) {
                4
            } else {
                3
            },
            if matches!(result.as_ref(), HirType::Array(inner) if **inner == HirType::String) { 1 }
            else if crate::waffle_backend::values::is_boxed_union(&result) { 2 }
            else if crate::waffle_backend::text_or_bytes::is_text_or_bytes(&result) { 3 }
            else if matches!(result.as_ref(), HirType::Union(types) if types.len()==2 && types.contains(&HirType::Number) && types.contains(&HirType::Void)) { 4 }
            else if crate::waffle_backend::values::sentinel_inner(&result).is_some() { 5 }
            else { 0 },
            u32::from(matches!(&input_ty, HirType::Array(inner) if **inner == HirType::String)),
        ]
        .map(|value| self.op(Operator::I32Const { value }, &[], &[Type::I32]));
        let function = self
            .registry
            .promises
            .as_ref()
            .map(|runtime| &runtime.native)
            .expect("combinators use resolved P3 threads")
            .combine
            .unwrap();
        let result = self.op(
            Operator::Call {
                function_index: function,
            },
            &[inputs[0], input, tags, inputs[1], inputs[2], inputs[3]],
            &[Type::I32],
        );
        self.reference_values.insert(result);
        Ok(result)
    }
}
