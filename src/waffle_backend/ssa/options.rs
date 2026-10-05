//! Recover source-order fields from plain literals validated by the frontend.

use super::FunctionLowerer;
use crate::waffle_backend::abi;
use anyhow::{Result, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{Operator, Type, Value};

use crate::waffle_backend::resolve::ResolvedContract;

pub(super) fn literal_properties<'a>(
    contract: &'a ResolvedContract,
    expression: &'a Expr,
) -> Result<Option<impl ExactSizeIterator<Item = (&'a str, &'a Expr)> + Clone>> {
    let properties = match expression {
        Expr::Object(properties) => LiteralProperties::Object(properties),
        Expr::New {
            class_name, args, ..
        } if contract.literal_shapes.contains_key(class_name) => {
            let fields = &contract.literal_shapes[class_name];
            ensure!(
                fields.len() == args.len(),
                "Literal object shape does not match its values"
            );
            LiteralProperties::Constructed(fields, args)
        }
        _ => return Ok(None),
    };
    let length = match properties {
        LiteralProperties::Object(fields) => fields.len(),
        LiteralProperties::Constructed(fields, _) => fields.len(),
    };
    Ok(Some((0..length).map(move |index| match properties {
        LiteralProperties::Object(fields) => (fields[index].0.as_str(), &fields[index].1),
        LiteralProperties::Constructed(fields, values) => (fields[index].as_str(), &values[index]),
    })))
}

#[derive(Clone, Copy)]
enum LiteralProperties<'a> {
    Object(&'a [(String, Expr)]),
    Constructed(&'a [String], &'a [Expr]),
}

impl FunctionLowerer<'_> {
    pub(super) fn option_signal(
        &mut self,
        object: Value,
        ty: &HirType,
        optional: bool,
        api: &str,
    ) -> Result<Value> {
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let (ty, optional) = super::types::optional_field_type(ty, optional);
        ensure!(
            ty == &HirType::Void
                || crate::waffle_backend::abort::Kind::of(ty)
                    == Some(crate::waffle_backend::abort::Kind::Signal),
            "{api} signal must be AbortSignal or undefined"
        );
        ensure!(
            self.registry.operations.is_some(),
            "{api} signals require acknowledged native operation ownership; public stream compositions are still being integrated"
        );
        let helpers = self.registry.object_helpers.unwrap();
        let key = self.expression(&Expr::String("signal".into()))?;
        let entry = self.op(
            Operator::Call {
                function_index: helpers.get,
            },
            &[object, key],
            &[Type::I32],
        );
        let tag = self.op(
            Operator::I32Const {
                value: crate::waffle_backend::values::ValueTag::of(ty)? as u32,
            },
            &[],
            &[Type::I32],
        );
        let optional = self.op(
            Operator::I32Const {
                value: u32::from(optional),
            },
            &[],
            &[Type::I32],
        );
        let payload = self.call_completion(helpers.value, &[entry, tag, optional]);
        let signal = abi::decode_payload(&mut self.body, self.block, payload, true);
        if let Some(helpers) = self.registry.abort_helpers {
            Ok(self.op(
                Operator::Call {
                    function_index: helpers.root,
                },
                &[signal],
                &[Type::I32],
            ))
        } else {
            Ok(zero)
        }
    }
}
