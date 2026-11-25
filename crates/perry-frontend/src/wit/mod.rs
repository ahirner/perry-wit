//! Source types and ownership helpers for a resolved component contract.
mod types;
pub(crate) use types::supported;

use perry_contract::{
    names::{canonical_type, to_camel_case, to_pascal_case},
    types::Input,
};
use pliron::builtin::types::Signedness;
use std::collections::HashMap;
use wit_parser::{FunctionKind, Handle, Resolve, Type, TypeDefKind, TypeId, WorldId};

#[derive(Clone, Copy)]
pub(crate) struct WitValue<'r> {
    pub(crate) resolve: &'r Resolve,
    pub(crate) ty: Type,
}

impl<'r> WitValue<'r> {
    pub(crate) fn result(self) -> Option<&'r wit_parser::Result_> {
        perry_contract::result_type(self.resolve, self.ty)
    }

    pub(crate) fn integer(self) -> Option<(u32, Signedness)> {
        let ty = match self.ty {
            Type::Id(id) => match self.resolve.types[canonical_type(self.resolve, id)].kind {
                TypeDefKind::Type(ty) => ty,
                _ => return None,
            },
            ty => ty,
        };
        Some(match ty {
            Type::U8 => (8, Signedness::Unsigned),
            Type::S8 => (8, Signedness::Signed),
            Type::U16 => (16, Signedness::Unsigned),
            Type::S16 => (16, Signedness::Signed),
            Type::U32 => (32, Signedness::Unsigned),
            Type::S32 => (32, Signedness::Signed),
            Type::U64 => (64, Signedness::Unsigned),
            Type::S64 => (64, Signedness::Signed),
            _ => return None,
        })
    }

    pub(crate) fn resource(self) -> Option<u32> {
        let Type::Id(id) = self.ty else { return None };
        let id = canonical_type(self.resolve, id);
        let owner = match self.resolve.types[id].kind {
            TypeDefKind::Resource => id,
            TypeDefKind::Handle(Handle::Own(owner) | Handle::Borrow(owner)) => {
                canonical_type(self.resolve, owner)
            }
            _ => return None,
        };
        Some(owner.index() as u32)
    }
}

pub(crate) struct ImportSignature<'r> {
    pub(crate) params: Vec<WitValue<'r>>,
    pub(crate) result: Option<WitValue<'r>>,
    pub(crate) is_async: bool,
}

pub(crate) enum Foreign<'r> {
    Type(WitValue<'r>),
    Input(WitValue<'r>),
    Function(ImportSignature<'r>),
}

#[derive(Default)]
pub struct WorldTypes<'r> {
    foreign: HashMap<String, Foreign<'r>>,
    members: Vec<ResourceMember<'r>>,
}

impl<'r> WorldTypes<'r> {
    pub fn new(resolve: &'r Resolve, world: Option<WorldId>) -> Self {
        Self {
            foreign: types::foreign(resolve, world),
            members: world
                .map(|id| resource_members(resolve, id))
                .unwrap_or_default(),
        }
    }

    pub(crate) fn foreign(&self) -> &HashMap<String, Foreign<'r>> {
        &self.foreign
    }

    pub(crate) fn member(
        &self,
        owner: u32,
        kind: MemberKind,
        name: &str,
    ) -> Option<&ResourceMember<'r>> {
        self.members.iter().find(|member| {
            member.owner == owner
                && member.kind == kind
                && (kind == MemberKind::Constructor || member.name == name)
        })
    }

    pub fn import_function(
        &mut self,
        resolve: &'r Resolve,
        module: &str,
        name: &str,
        params: impl IntoIterator<Item = Type>,
        result: Option<Type>,
        is_async: bool,
    ) -> String {
        let binding = format!("{module}#{name}");
        self.foreign.insert(
            binding.clone(),
            Foreign::Function(ImportSignature {
                params: params
                    .into_iter()
                    .map(|ty| WitValue { resolve, ty })
                    .collect(),
                result: result.map(|ty| WitValue { resolve, ty }),
                is_async,
            }),
        );
        binding
    }
}

#[derive(PartialEq, Eq)]
pub(crate) enum MemberKind {
    Instance,
    Static,
    Constructor,
    Dispose,
}

pub(crate) struct ResourceMember<'r> {
    pub(crate) owner: u32,
    pub(crate) kind: MemberKind,
    pub(crate) name: String,
    pub(crate) binding: String,
    pub(crate) result: Option<WitValue<'r>>,
}

pub(crate) fn owned_resource(resolve: &Resolve, ty: Type) -> Option<TypeId> {
    let Type::Id(id) = ty else { return None };
    let id = canonical_type(resolve, id);
    match resolve.types[id].kind {
        TypeDefKind::Handle(Handle::Own(owner)) => Some(canonical_type(resolve, owner)),
        TypeDefKind::Resource => Some(id),
        _ => None,
    }
}

fn imported_resource_member<'r>(
    resolve: &'r Resolve,
    entry: perry_contract::WorldFunction<'_>,
) -> Option<ResourceMember<'r>> {
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
    if !function
        .params
        .iter()
        .all(|p| types::supported(resolve, Input::Reference(p.ty)))
    {
        return None;
    }
    let result = function
        .result
        .and_then(|ty| perry_contract::result_type(resolve, ty).map_or(Some(ty), |sum| sum.ok))
        .map(|ty| WitValue { resolve, ty });
    Some(ResourceMember {
        owner: canonical_type(resolve, owner).index() as u32,
        kind,
        name: to_camel_case(function.item_name()),
        binding: format!(
            "{}#{}",
            resolve.name_world_key(entry.key),
            to_camel_case(&function.name)
        ),
        result,
    })
}

fn resource_members(resolve: &Resolve, world: WorldId) -> Vec<ResourceMember<'_>> {
    let mut members: Vec<_> =
        perry_contract::world_functions(resolve, resolve.worlds[world].imports.iter())
            .filter_map(|entry| imported_resource_member(resolve, entry))
            .collect();
    for (key, item) in &resolve.worlds[world].imports {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        for (name, id) in &resolve.interfaces[*id].types {
            if matches!(resolve.types[*id].kind, wit_parser::TypeDefKind::Resource) {
                members.push(ResourceMember {
                    owner: id.index() as u32,
                    kind: MemberKind::Dispose,
                    name: String::new(),
                    binding: format!(
                        "{}#drop{}",
                        resolve.name_world_key(key),
                        to_pascal_case(name)
                    ),
                    result: None,
                });
            }
        }
    }
    members
}
