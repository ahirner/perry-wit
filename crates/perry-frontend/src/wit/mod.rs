//! Source types and ownership helpers for a resolved component contract.
mod types;

use crate::hir::{Foreign, FunctionType, Type as HirType};
use anyhow::{Context, Result};
use perry_contract::{
    names::{NamedType, canonical_type, to_camel_case, to_pascal_case},
    types::Input,
};
use std::collections::HashMap;
use wit_parser::{Function, Resolve, TypeId, WorldId};

#[derive(Default)]
pub struct WorldTypes {
    foreign: HashMap<String, Foreign>,
}

pub enum ResourceOperation {
    Drop,
    New,
    Representation,
}

pub struct ResourceHelper {
    pub binding: String,
    pub name: String,
    pub operation: ResourceOperation,
}

impl WorldTypes {
    pub fn new(resolve: &Resolve, world: Option<WorldId>) -> Self {
        Self {
            foreign: types::foreign(resolve, world),
        }
    }

    pub fn foreign(&self) -> &HashMap<String, Foreign> {
        &self.foreign
    }

    pub fn import_function(
        &mut self,
        resolve: &Resolve,
        module: &str,
        function: &Function,
    ) -> Option<String> {
        let params = function
            .params
            .iter()
            .map(|parameter| {
                Some((
                    parameter.name.clone(),
                    HirType::Generic {
                        base: "WitInput".into(),
                        type_args: vec![types::ty(resolve, Input::Reference(parameter.ty))?],
                    },
                    false,
                ))
            })
            .collect::<Option<_>>()?;
        let result = match function.result {
            Some(ty) => types::ty(resolve, Input::Reference(ty))?,
            None => HirType::Void,
        };
        let binding = format!("{module}#{}", to_camel_case(&function.name));
        self.foreign.insert(
            binding.clone(),
            Foreign::Function(FunctionType {
                params,
                return_type: Box::new(result),
                is_async: function.kind.is_async(),
                is_generator: false,
            }),
        );
        Some(binding)
    }

    pub fn resource_helpers(
        &mut self,
        resolve: &Resolve,
        id: TypeId,
        module: &str,
        name: &str,
        exported: bool,
    ) -> Result<Vec<ResourceHelper>> {
        let resource = source_type(resolve, id).context("resource has no WIT declaration name")?;
        let suffix = to_pascal_case(name);
        let mut helpers = vec![(
            format!("drop{suffix}"),
            ResourceOperation::Drop,
            resource.clone(),
            HirType::Void,
        )];
        if exported {
            helpers.extend([
                (
                    format!("new{suffix}"),
                    ResourceOperation::New,
                    HirType::Number,
                    resource.clone(),
                ),
                (
                    format!("{}Rep", to_camel_case(name)),
                    ResourceOperation::Representation,
                    resource,
                    HirType::Number,
                ),
            ]);
        }
        Ok(helpers
            .into_iter()
            .map(|(name, operation, input, output)| {
                let binding = format!("{module}#{name}");
                self.foreign.insert(
                    binding.clone(),
                    Foreign::Function(FunctionType {
                        params: vec![("value".into(), input, false)],
                        return_type: Box::new(output),
                        is_async: false,
                        is_generator: false,
                    }),
                );
                ResourceHelper {
                    binding,
                    name,
                    operation,
                }
            })
            .collect())
    }
}

fn source_type(resolve: &Resolve, id: TypeId) -> Option<HirType> {
    Some(HirType::Named(
        NamedType::get(resolve, canonical_type(resolve, id))?.qualified(),
    ))
}
