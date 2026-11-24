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

pub(crate) fn resource_members(resolve: &Resolve, world: WorldId) -> Vec<ResourceMember> {
    use wit_parser::FunctionKind;
    let mut members: Vec<_> =
        perry_contract::world_functions(resolve, resolve.worlds[world].imports.iter())
            .filter_map(|entry| {
                let f = entry.function;
                let id = f.kind.resource()?;
                if f.params.iter().any(|p| matches!(p.ty, wit_parser::Type::Id(id)
                    if matches!(resolve.types[canonical_type(resolve,id)].kind, wit_parser::TypeDefKind::Flags(_)))) {
                    return None;
                }
                let kind = match f.kind {
                    FunctionKind::Method(_) => MemberKind::Instance,
                    FunctionKind::Static(_) => MemberKind::Static,
                    FunctionKind::Constructor(_) => MemberKind::Constructor,
                    _ => return None,
                };
                let ty = |t| types::ty(resolve, Input::Reference(t));
                let raw = f.result.and_then(ty);
                let sum = f
                    .result
                    .and_then(|t| perry_contract::result_type(resolve, t));
                let result = if let Some(sum) = sum {
                    sum.ok.and_then(ty)
                } else {
                    raw.clone()
                };
                Some(ResourceMember {
                    owner: canonical_type(resolve, id).index() as u32,
                    kind,
                    name: to_camel_case(f.item_name()),
                    binding: format!(
                        "{}#{}",
                        resolve.name_world_key(entry.key),
                        to_camel_case(&f.name)
                    ),
                    params: f.params.iter().map(|p| ty(p.ty)).collect::<Option<_>>()?,
                    result_inputs:f.params.iter().enumerate().filter_map(|(index,p)| {
                        let sum=perry_contract::result_type(resolve,p.ty)?;
                        let mut cases=Vec::new();
                        for (tag,payload) in [("ok",sum.ok),("err",sum.err)] {
                            let case=types::object(std::iter::once(Some(("tag".into(),HirType::StringLiteral(tag.into()))))
                                .chain(payload.map(|t|Some(("val".into(),ty(t)?)))))?;
                            cases.push(case);
                        }
                        Some(ResultInput{index,tagged:HirType::Union(cases),has_ok:sum.ok.is_some(),has_err:sum.err.is_some()})
                    }).collect(),
                    result: result.unwrap_or(HirType::Void),
                    raw_result: sum.and(raw),
                    error: sum.and_then(|r| r.err).and_then(|err| {
                        let wit_parser::Type::Id(id) = err else {return Some(ResourceError::Value)};
                        let wit_parser::TypeDefKind::Variant(v) = &resolve.types[canonical_type(resolve,id)].kind else {return Some(ResourceError::Value)};
                        let owned = v.cases.iter().filter_map(|case| {
                            let wit_parser::Type::Id(id) = case.ty? else {return None};
                            let id=canonical_type(resolve,id);
                            let owner=match resolve.types[id].kind {
                                wit_parser::TypeDefKind::Handle(wit_parser::Handle::Own(owner))=>canonical_type(resolve,owner),
                                wit_parser::TypeDefKind::Resource=>id,
                                _=>return None,
                            };
                            Some((case.name.clone(),owner.index() as u32))
                        }).collect();
                        Some(ResourceError::Tagged {ty:ty(err)?,owned})
                    }),
                })
            })
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
