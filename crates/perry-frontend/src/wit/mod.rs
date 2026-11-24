//! Source types and ownership helpers for a resolved component contract.
mod types;
pub(crate) use types::ty as source_type_of;

use crate::hir::{Foreign, FunctionType, Type as HirType};
use perry_contract::{
    names::{NamedType, canonical_type, to_camel_case, to_pascal_case},
    types::Input,
};
use perry_wit_diagnostics::{Diagnostic, Result};
use std::collections::HashMap;
use wit_parser::{Function, FunctionKind, Handle, Resolve, Type, TypeDefKind, TypeId, WorldId};

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
        let resource = source_type(resolve, id)
            .ok_or_else(|| Diagnostic::internal("resource has no WIT declaration name"))?;
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

pub(crate) fn source_type(resolve: &Resolve, id: TypeId) -> Option<HirType> {
    Some(HirType::Named(
        NamedType::get(resolve, canonical_type(resolve, id))?.qualified(),
    ))
}

#[derive(PartialEq, Eq)]
pub(crate) enum MemberKind {
    Instance,
    Static,
    Constructor,
    Dispose,
}

pub(crate) enum ResourceError {
    Value,
    Tagged {
        ty: HirType,
        owned: Vec<(String, u32)>,
    },
}
pub(crate) struct ResultInput {
    pub(crate) index: usize,
    pub(crate) tagged: HirType,
    pub(crate) has_ok: bool,
    pub(crate) has_err: bool,
}

pub(crate) struct ResourceMember {
    pub(crate) owner: u32,
    pub(crate) kind: MemberKind,
    pub(crate) name: String,
    pub(crate) binding: String,
    pub(crate) params: Vec<HirType>,
    pub(crate) result: HirType,
    pub(crate) result_inputs: Vec<ResultInput>,
    pub(crate) raw_result: Option<HirType>,
    pub(crate) error: Option<ResourceError>,
}

fn result_input(resolve: &Resolve, index: usize, ty: Type) -> Option<ResultInput> {
    let result = perry_contract::result_type(resolve, ty)?;
    let mut cases = Vec::new();
    for (tag, payload) in [("ok", result.ok), ("err", result.err)] {
        let tag = ("tag".into(), HirType::StringLiteral(tag.into()));
        let value =
            payload.map(|ty| Some(("val".into(), types::ty(resolve, Input::Reference(ty))?)));
        cases.push(types::object(std::iter::once(Some(tag)).chain(value))?);
    }
    Some(ResultInput {
        index,
        tagged: HirType::Union(cases),
        has_ok: result.ok.is_some(),
        has_err: result.err.is_some(),
    })
}

fn owned_resource(resolve: &Resolve, ty: Type) -> Option<TypeId> {
    let Type::Id(id) = ty else { return None };
    let id = canonical_type(resolve, id);
    match resolve.types[id].kind {
        TypeDefKind::Handle(Handle::Own(owner)) => Some(canonical_type(resolve, owner)),
        TypeDefKind::Resource => Some(id),
        _ => None,
    }
}

fn resource_error(resolve: &Resolve, ty: Type) -> Option<ResourceError> {
    let Type::Id(id) = ty else {
        return Some(ResourceError::Value);
    };
    let TypeDefKind::Variant(variant) = &resolve.types[canonical_type(resolve, id)].kind else {
        return Some(ResourceError::Value);
    };
    let owned = variant
        .cases
        .iter()
        .filter_map(|case| {
            let owner = owned_resource(resolve, case.ty?)?;
            Some((case.name.clone(), owner.index() as u32))
        })
        .collect();
    Some(ResourceError::Tagged {
        ty: types::ty(resolve, Input::Reference(ty))?,
        owned,
    })
}

fn imported_resource_member(
    resolve: &Resolve,
    entry: perry_contract::WorldFunction<'_>,
) -> Option<ResourceMember> {
    let function = entry.function;
    let owner = function.kind.resource()?;
    let kind = match function.kind {
        FunctionKind::Method(_) => MemberKind::Instance,
        FunctionKind::Static(_) => MemberKind::Static,
        FunctionKind::Constructor(_) => MemberKind::Constructor,
        _ => return None,
    };
    if function.params.iter().any(|param| {
        let Type::Id(id) = param.ty else { return false };
        matches!(
            resolve.types[canonical_type(resolve, id)].kind,
            TypeDefKind::Flags(_)
        )
    }) {
        return None;
    }
    let ty = |t| types::ty(resolve, Input::Reference(t));
    let raw = function.result.and_then(ty);
    let sum = function
        .result
        .and_then(|t| perry_contract::result_type(resolve, t));
    let (result, raw_result) = match sum {
        Some(sum) => (sum.ok.and_then(ty), raw),
        None => (raw, None),
    };
    Some(ResourceMember {
        owner: canonical_type(resolve, owner).index() as u32,
        kind,
        name: to_camel_case(function.item_name()),
        binding: format!(
            "{}#{}",
            resolve.name_world_key(entry.key),
            to_camel_case(&function.name)
        ),
        params: function
            .params
            .iter()
            .map(|param| ty(param.ty))
            .collect::<Option<_>>()?,
        result_inputs: function
            .params
            .iter()
            .enumerate()
            .filter_map(|(index, param)| result_input(resolve, index, param.ty))
            .collect(),
        result: result.unwrap_or(HirType::Void),
        raw_result,
        error: sum.and_then(|sum| resource_error(resolve, sum.err?)),
    })
}

pub(crate) fn resource_members(resolve: &Resolve, world: WorldId) -> Vec<ResourceMember> {
    let mut members: Vec<_> =
        perry_contract::world_functions(resolve, resolve.worlds[world].imports.iter())
            .filter_map(|entry| imported_resource_member(resolve, entry))
            .collect();
    for (key, item) in &resolve.worlds[world].imports {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        for (name, id) in &resolve.interfaces[*id].types {
            if matches!(resolve.types[*id].kind, wit_parser::TypeDefKind::Resource)
                && let Some(ty) = source_type(resolve, *id)
            {
                members.push(ResourceMember {
                    owner: id.index() as u32,
                    kind: MemberKind::Dispose,
                    name: String::new(),
                    binding: format!(
                        "{}#drop{}",
                        resolve.name_world_key(key),
                        to_pascal_case(name)
                    ),
                    params: vec![ty],
                    result: HirType::Void,
                    result_inputs: vec![],
                    raw_result: None,
                    error: None,
                });
            }
        }
    }
    members
}
